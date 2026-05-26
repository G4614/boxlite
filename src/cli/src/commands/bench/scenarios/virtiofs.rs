//! virtiofs throughput — write into a host-shared volume.
//!
//! `-v /host/dir:/container/dir` plumbs a host directory into the
//! guest via virtiofs. This code path is independent of the qcow2
//! COW overlay (`throughput-disk-write`'s target). Workloads that
//! mount user volumes (compose stacks, CI sandboxes with cache
//! dirs) hit this path; a regression in virtiofs version or
//! shared-mount setup would surface here.
//!
//! Architecture:
//!   1. Host: TempDir for the shared volume.
//!   2. Box created with `volumes: vec![VolumeSpec { host_path,
//!      guest_path: "/host", ... }]`.
//!   3. In-box: `dd if=/dev/zero of=/host/bench-virtiofs bs=1M
//!      count=64 conv=fsync`.
//!   4. Parse dd's MB/s line.
//!   5. After: stat the host file size to confirm bytes landed.

use super::super::runner::{RunContext, Scenario};
use super::common::{BoxGuard, build_runtime};
use anyhow::{Context, Result};
use async_trait::async_trait;
use boxlite::runtime::options::{RootfsSpec, VolumeSpec};
use boxlite::{BoxCommand, BoxOptions};
use futures::StreamExt;
use std::collections::BTreeMap;
use std::time::Instant;
use tempfile::TempDir;

const WRITE_MIB: u32 = 64;

pub struct Virtiofs {
    home: Option<TempDir>,
    shared: Option<TempDir>,
}

impl Virtiofs {
    pub fn new() -> Self {
        Self {
            home: None,
            shared: None,
        }
    }
}

#[async_trait]
impl Scenario for Virtiofs {
    fn name(&self) -> &str {
        "throughput-virtiofs"
    }

    async fn run_once(&mut self, ctx: &RunContext) -> Result<BTreeMap<String, f64>> {
        if self.home.is_none() {
            self.home = Some(TempDir::new().context("mkdir virtiofs home")?);
        }
        if self.shared.is_none() {
            self.shared = Some(TempDir::new().context("mkdir virtiofs shared")?);
        }
        let home_path = self
            .home
            .as_ref()
            .expect("just initialized")
            .path()
            .to_path_buf();
        let shared_path = self
            .shared
            .as_ref()
            .expect("just initialized")
            .path()
            .to_path_buf();
        let rt = build_runtime(ctx.global, home_path)?;

        let opts = BoxOptions {
            rootfs: RootfsSpec::Image("alpine:latest".into()),
            auto_remove: true,
            volumes: vec![VolumeSpec {
                host_path: shared_path.to_string_lossy().into_owned(),
                guest_path: "/host".to_string(),
                read_only: false,
            }],
            ..Default::default()
        };
        let live = rt.create(opts, None).await.context("rt.create(alpine)")?;
        let mut guard = BoxGuard::new(&rt, live.id().to_string());
        live.start().await.context("box.start()")?;

        let cmd = BoxCommand::new("dd").args([
            "if=/dev/zero",
            "of=/host/bench-virtiofs",
            "bs=1M",
            &format!("count={WRITE_MIB}"),
            "conv=fsync",
        ]);
        let exec_start = Instant::now();
        let mut exec = live.exec(cmd).await.context("box.exec(dd virtiofs)")?;
        let mut stderr = exec.stderr().expect("stderr handle");
        let mut stderr_text = String::new();
        while let Some(chunk) = stderr.next().await {
            stderr_text.push_str(&chunk);
        }
        if let Some(mut s) = exec.stdout() {
            tokio::spawn(async move { while s.next().await.is_some() {} });
        }
        let r = exec.wait().await.context("dd virtiofs wait")?;
        if r.exit_code != 0 {
            anyhow::bail!(
                "dd to /host exited non-zero ({}); stderr:\n{stderr_text}",
                r.exit_code
            );
        }
        let wall_ms = exec_start.elapsed().as_secs_f64() * 1000.0;
        let mb_per_sec = parse_dd_mb_per_sec(&stderr_text)
            .with_context(|| format!("parse dd:\n{stderr_text}"))?;

        // Confirm bytes really landed on the host side.
        let host_file = shared_path.join("bench-virtiofs");
        let host_bytes = std::fs::metadata(&host_file).map(|m| m.len()).unwrap_or(0);
        let _ = std::fs::remove_file(&host_file);

        live.stop().await.context("box.stop()")?;
        guard.disarm();

        let mut metrics = BTreeMap::new();
        metrics.insert("virtiofs_write_mb_per_sec".into(), mb_per_sec);
        metrics.insert(
            "virtiofs_write_bytes".into(),
            (WRITE_MIB as u64 * 1024 * 1024) as f64,
        );
        metrics.insert("virtiofs_host_observed_bytes".into(), host_bytes as f64);
        metrics.insert("virtiofs_write_wall_ms".into(), wall_ms);
        Ok(metrics)
    }
}

/// Same MB/s parser as `throughput-disk-write` / `disk-read` —
/// duplicated here for module isolation. See `disk_read::tests`
/// for the format coverage.
fn parse_dd_mb_per_sec(text: &str) -> Result<f64> {
    for line in text.lines().rev() {
        let (unit_pos, multiplier) = if let Some(pos) = line.rfind("MB/s") {
            (pos, 1.0)
        } else if let Some(pos) = line.rfind("GB/s") {
            (pos, 1024.0)
        } else if let Some(pos) = line.rfind("kB/s") {
            (pos, 1.0 / 1024.0)
        } else {
            continue;
        };
        let head = &line[..unit_pos].trim_end();
        let tok = head
            .split(|c: char| c.is_whitespace() || c == ',')
            .next_back()
            .unwrap_or("");
        if let Ok(v) = tok.parse::<f64>() {
            return Ok(v * multiplier);
        }
    }
    anyhow::bail!("no parseable rate line in dd output")
}

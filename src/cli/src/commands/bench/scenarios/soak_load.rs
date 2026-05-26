//! Soak-under-load — catches leak classes idle soak misses.
//!
//! `stability-soak` keeps an alpine box alive for N seconds and
//! checks for slow growth in RSS / COW / fd. That catches leaks
//! that fire from the IDLE path (a goroutine that grows a buffer
//! on every keepalive, a timer that never frees) — but misses
//! everything that only fires under traffic:
//!
//!   * gvproxy goroutine pools that grow per connection and don't
//!     shrink after the connection closes;
//!   * libkrun's KVM dirty-page tracking buffers that bloat when
//!     the guest is actually using memory bandwidth;
//!   * the guest agent's exec-result reaper that has to clean up
//!     completed exec records — only stressed when execs happen;
//!   * shim-side `BoxState::port_mappings` accumulation in a
//!     hypothetical version that didn't clear on stop (regression
//!     guard for the 568 fix).
//!
//! This scenario keeps a workload running for the duration of the
//! soak. The workload is a `sh` loop that fires a `fio` random-
//! read iteration each second — light enough to not pin a vCPU
//! but heavy enough to keep the disk-read path, exec-reaper, and
//! virtio-blk packet ring all hot. Sample BoxMetrics every 2 s.
//!
//! Env var `BOXLITE_BENCH_SOAK_SECS` (default 30 s) controls the
//! soak length — same knob as `stability-soak` for consistency.
//!
//! Reports:
//!   * `soak_load_secs` — actual elapsed.
//!   * `rss_load_growth_bytes` — last RSS minus first.
//!   * `rss_load_max_bytes` — peak RSS observed.
//!   * `cow_load_growth_bytes` — last COW minus first.
//!   * `fd_load_growth_count` — host `/proc/self/fd` delta.
//!   * `workload_cycles_completed_count` — how many fio cycles ran
//!     during the soak (sanity: if it's 0 the soak ran without
//!     traffic, which would make this scenario behave like
//!     `stability-soak` and the data is meaningless).

use super::super::runner::{RunContext, Scenario};
use super::common::{BoxGuard, alpine_options, build_runtime};
use anyhow::{Context, Result};
use async_trait::async_trait;
use boxlite::BoxCommand;
use futures::StreamExt;
use std::collections::BTreeMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tempfile::TempDir;

const DEFAULT_SOAK_SECS: u64 = 30;
const SAMPLE_INTERVAL: Duration = Duration::from_secs(2);
const SOAK_SECS_ENV: &str = "BOXLITE_BENCH_SOAK_SECS";

fn configured_soak_secs() -> u64 {
    std::env::var(SOAK_SECS_ENV)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|s| *s >= 1)
        .unwrap_or(DEFAULT_SOAK_SECS)
}

pub struct SoakLoad {
    home: Option<TempDir>,
    fio_installed: bool,
}

impl SoakLoad {
    pub fn new() -> Self {
        Self {
            home: None,
            fio_installed: false,
        }
    }
}

#[async_trait]
impl Scenario for SoakLoad {
    fn name(&self) -> &str {
        "stability-soak-load"
    }

    async fn run_once(&mut self, ctx: &RunContext) -> Result<BTreeMap<String, f64>> {
        let soak_secs = configured_soak_secs();

        if self.home.is_none() {
            self.home = Some(TempDir::new().context("mkdir soak-load home")?);
        }
        let home_path = self
            .home
            .as_ref()
            .expect("just initialized")
            .path()
            .to_path_buf();
        let rt = build_runtime(ctx.global, home_path.clone())?;

        let live = rt
            .create(alpine_options(), None)
            .await
            .context("rt.create(alpine)")?;
        let box_id = live.id().to_string();
        let mut guard = BoxGuard::new(&rt, box_id.clone());
        live.start().await.context("box.start()")?;

        if !self.fio_installed {
            let install = BoxCommand::new("apk").args(["add", "--no-cache", "fio"]);
            let mut exec = live.exec(install).await.context("apk add fio")?;
            if let Some(mut s) = exec.stdout() {
                tokio::spawn(async move { while s.next().await.is_some() {} });
            }
            if let Some(mut s) = exec.stderr() {
                tokio::spawn(async move { while s.next().await.is_some() {} });
            }
            let r = exec.wait().await.context("apk add fio wait")?;
            if r.exit_code != 0 {
                anyhow::bail!("apk add fio failed (exit {})", r.exit_code);
            }
            self.fio_installed = true;
        }

        // Pre-create the workload file once so each cycle doesn't
        // pay a fresh write before the read. Light fio random-read
        // loop afterwards. `--runtime` per cycle is the same as
        // SAMPLE_INTERVAL so the cycle count roughly matches the
        // sample count.
        let workload_script = format!(
            "set -e; \
             dd if=/dev/zero of=/tmp/soak-data bs=1M count=32 2>/dev/null; \
             count=0; \
             end=$(($(date +%s) + {soak_secs})); \
             while [ \"$(date +%s)\" -lt \"$end\" ]; do \
                fio --name=randread --rw=randread --bs=4K --size=32M --runtime=1 \
                    --time_based --ioengine=sync --filename=/tmp/soak-data \
                    --output-format=json >/dev/null 2>&1 || break; \
                count=$((count + 1)); \
             done; \
             echo \"workload_cycles=$count\""
        );
        let workload_cmd = BoxCommand::new("sh").args(["-c", &workload_script]);
        let mut workload_exec = live
            .exec(workload_cmd)
            .await
            .context("box.exec(workload)")?;

        // Capture stdout so we can read the final cycle count.
        let mut stdout = workload_exec.stdout().expect("stdout handle");
        let stdout_buf = std::sync::Arc::new(tokio::sync::Mutex::new(String::new()));
        let stdout_writer = std::sync::Arc::clone(&stdout_buf);
        let stdout_drain = tokio::spawn(async move {
            while let Some(chunk) = stdout.next().await {
                stdout_writer.lock().await.push_str(&chunk);
            }
        });
        if let Some(mut s) = workload_exec.stderr() {
            tokio::spawn(async move { while s.next().await.is_some() {} });
        }

        // Brief settle so the first fio cycle isn't still spinning
        // up when the first sample fires.
        tokio::time::sleep(Duration::from_millis(500)).await;

        let fd_before = host_fd_count();
        let mut rss_samples: Vec<u64> = Vec::new();
        let mut cow_samples: Vec<u64> = Vec::new();

        let deadline = Instant::now() + Duration::from_secs(soak_secs);
        loop {
            let snap = live
                .metrics()
                .await
                .context("snap BoxMetrics during soak-load")?;
            if let Some(r) = snap.memory_bytes() {
                rss_samples.push(r);
            }
            if let Some(c) = cow_disk_size(&home_path, &box_id) {
                cow_samples.push(c);
            }
            if Instant::now() >= deadline {
                break;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            let sleep_for = remaining.min(SAMPLE_INTERVAL);
            if sleep_for.is_zero() {
                break;
            }
            tokio::time::sleep(sleep_for).await;
        }
        let fd_after = host_fd_count();

        // Workload exits naturally once `date +%s >= end`; bound
        // the wait anyway.
        let _ = tokio::time::timeout(Duration::from_secs(5), workload_exec.wait()).await;
        let _ = stdout_drain.await;
        let stdout_text = stdout_buf.lock().await.clone();
        let workload_cycles = stdout_text
            .lines()
            .find_map(|l| l.strip_prefix("workload_cycles="))
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(0);

        live.stop().await.context("box.stop()")?;
        guard.disarm();

        let mut metrics = BTreeMap::new();
        metrics.insert("soak_load_secs".into(), soak_secs as f64);
        if let (Some(&first), Some(&last)) = (rss_samples.first(), rss_samples.last()) {
            metrics.insert("rss_load_growth_bytes".into(), last as f64 - first as f64);
        }
        if let Some(&peak) = rss_samples.iter().max() {
            metrics.insert("rss_load_max_bytes".into(), peak as f64);
        }
        if let (Some(&first), Some(&last)) = (cow_samples.first(), cow_samples.last()) {
            metrics.insert("cow_load_growth_bytes".into(), last as f64 - first as f64);
        }
        if let (Some(before), Some(after)) = (fd_before, fd_after) {
            metrics.insert("fd_load_growth_count".into(), after as f64 - before as f64);
        }
        metrics.insert(
            "workload_cycles_completed_count".into(),
            workload_cycles as f64,
        );
        Ok(metrics)
    }
}

fn host_fd_count() -> Option<usize> {
    Some(std::fs::read_dir("/proc/self/fd").ok()?.count())
}

fn cow_disk_size(home: &Path, box_id: &str) -> Option<u64> {
    let path = PathBuf::from(home)
        .join("boxes")
        .join(box_id)
        .join("disks")
        .join("disk.qcow2");
    std::fs::metadata(&path).ok().map(|m| m.blocks() * 512)
}

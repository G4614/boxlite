//! Host↔box file copy throughput via `LiteBox::copy_into` /
//! `copy_out`. Tests the tar-streaming codepath the CLI's `cp`
//! command uses (`src/cli/src/commands/cp.rs`), which is a
//! distinct API surface from `boxlite exec` + `dd`.
//!
//! Two scenarios in one file:
//!   * `throughput-copy-into` — host → guest, 64 MiB.
//!   * `throughput-copy-out`  — guest → host, 64 MiB.
//!
//! Both pre-stage a 64 MiB host file on first iteration (for
//! copy-into) or in-box file (for copy-out). Subsequent
//! iterations reuse the stage so the throughput number is the
//! copy alone, not the prep.

use super::super::runner::{RunContext, Scenario};
use super::common::{alpine_options, build_runtime};
use anyhow::{Context, Result};
use async_trait::async_trait;
use boxlite::{BoxCommand, BoxliteRuntime, CopyOptions, LiteBox};
use futures::StreamExt;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Instant;
use tempfile::TempDir;

const COPY_BYTES: u64 = 64 * 1024 * 1024;

/// Stage `<host_src_dir>/payload` as a dense 64 MiB file. Used
/// by `throughput-copy-into`.
fn stage_host_file(dir: &std::path::Path) -> Result<PathBuf> {
    let path = dir.join("payload");
    let bytes = vec![0u8; COPY_BYTES as usize];
    std::fs::write(&path, &bytes).context("stage host payload")?;
    Ok(path)
}

/// Stage `/root/payload-out` as a dense 64 MiB file inside the
/// box. Used by `throughput-copy-out`.
async fn stage_box_file(live: &LiteBox) -> Result<()> {
    let cmd = BoxCommand::new("dd").args([
        "if=/dev/zero",
        "of=/root/payload-out",
        "bs=1M",
        "count=64",
        "conv=fsync",
    ]);
    let mut exec = live.exec(cmd).await.context("box.exec(dd stage)")?;
    if let Some(mut s) = exec.stdout() {
        tokio::spawn(async move { while s.next().await.is_some() {} });
    }
    if let Some(mut s) = exec.stderr() {
        tokio::spawn(async move { while s.next().await.is_some() {} });
    }
    let r = exec.wait().await.context("dd stage wait")?;
    if r.exit_code != 0 {
        anyhow::bail!("dd stage failed exit {}", r.exit_code);
    }
    Ok(())
}

/// Boot a fresh alpine box, run `body` against it, tear down.
/// Used by both scenarios so the per-iteration plumbing is in
/// one place.
async fn with_box<F, Fut>(rt: &BoxliteRuntime, body: F) -> Result<(LiteBox, BTreeMap<String, f64>)>
where
    F: FnOnce(LiteBox) -> Fut,
    Fut: std::future::Future<Output = Result<(LiteBox, BTreeMap<String, f64>)>>,
{
    let live = rt
        .create(alpine_options(), None)
        .await
        .context("rt.create(alpine)")?;
    live.start().await.context("box.start()")?;
    body(live).await
}

// ─── throughput-copy-into ────────────────────────────────────────

pub struct CopyInto {
    home: Option<TempDir>,
    host_stage: Option<TempDir>,
    payload_path: Option<PathBuf>,
}

impl CopyInto {
    pub fn new() -> Self {
        Self {
            home: None,
            host_stage: None,
            payload_path: None,
        }
    }
}

#[async_trait]
impl Scenario for CopyInto {
    fn name(&self) -> &str {
        "throughput-copy-into"
    }

    async fn run_once(&mut self, ctx: &RunContext) -> Result<BTreeMap<String, f64>> {
        if self.home.is_none() {
            self.home = Some(TempDir::new().context("mkdir copy-into home")?);
        }
        if self.host_stage.is_none() {
            self.host_stage = Some(TempDir::new().context("mkdir host stage")?);
        }
        if self.payload_path.is_none() {
            self.payload_path = Some(stage_host_file(
                self.host_stage.as_ref().expect("init").path(),
            )?);
        }
        let payload = self.payload_path.as_ref().expect("staged").clone();

        let home_path = self.home.as_ref().expect("init").path().to_path_buf();
        let rt = build_runtime(ctx.global, home_path)?;
        let (live, metrics) = with_box(&rt, |live| async move {
            let t0 = Instant::now();
            live.copy_into(&payload, "/tmp/copied-in", CopyOptions::default())
                .await
                .context("copy_into")?;
            let copy_ms = t0.elapsed().as_secs_f64() * 1000.0;
            let mut m = BTreeMap::new();
            m.insert("copy_into_ms".into(), copy_ms);
            m.insert("copy_into_bytes".into(), COPY_BYTES as f64);
            if copy_ms > 0.0 {
                let mib = COPY_BYTES as f64 / (1024.0 * 1024.0);
                m.insert("copy_into_mb_per_sec".into(), mib / (copy_ms / 1000.0));
            }
            Ok((live, m))
        })
        .await?;

        let _ = live.stop().await;
        Ok(metrics)
    }
}

// ─── throughput-copy-out ─────────────────────────────────────────

pub struct CopyOut {
    home: Option<TempDir>,
    host_dest: Option<TempDir>,
}

impl CopyOut {
    pub fn new() -> Self {
        Self {
            home: None,
            host_dest: None,
        }
    }
}

#[async_trait]
impl Scenario for CopyOut {
    fn name(&self) -> &str {
        "throughput-copy-out"
    }

    async fn run_once(&mut self, ctx: &RunContext) -> Result<BTreeMap<String, f64>> {
        if self.home.is_none() {
            self.home = Some(TempDir::new().context("mkdir copy-out home")?);
        }
        if self.host_dest.is_none() {
            self.host_dest = Some(TempDir::new().context("mkdir host dest")?);
        }
        let host_dest_path = self.host_dest.as_ref().expect("init").path().to_path_buf();

        let home_path = self.home.as_ref().expect("init").path().to_path_buf();
        let rt = build_runtime(ctx.global, home_path)?;
        let (live, metrics) = with_box(&rt, |live| async move {
            stage_box_file(&live).await?;
            let dest = host_dest_path.join("copied-out");
            let _ = std::fs::remove_file(&dest); // fresh each iter
            let t0 = Instant::now();
            live.copy_out("/root/payload-out", &dest, CopyOptions::default())
                .await
                .context("copy_out")?;
            let copy_ms = t0.elapsed().as_secs_f64() * 1000.0;
            let host_bytes = std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
            let _ = std::fs::remove_file(&dest);
            let mut m = BTreeMap::new();
            m.insert("copy_out_ms".into(), copy_ms);
            m.insert("copy_out_bytes".into(), host_bytes as f64);
            if copy_ms > 0.0 {
                let mib = host_bytes as f64 / (1024.0 * 1024.0);
                m.insert("copy_out_mb_per_sec".into(), mib / (copy_ms / 1000.0));
            }
            Ok((live, m))
        })
        .await?;

        let _ = live.stop().await;
        Ok(metrics)
    }
}

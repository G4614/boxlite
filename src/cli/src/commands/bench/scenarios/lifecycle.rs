//! Box-lifecycle scenarios: clone + export.
//!
//! These exercise APIs that don't show up in the cold/warm-start
//! latency suite because they're not the create/start path —
//! they're lifecycle ops on a started box.
//!
//! Two scenarios in this file (share the same source-box setup):
//!
//!   * `latency-clone` — per-iteration cost of `LiteBox::clone_box`
//!     on a 64 MiB-pre-written source. Cloning is the hot path
//!     for "spin up N variants of the same base" workflows; a
//!     regression in the COW-overlay setup or DB row materialize
//!     would show here.
//!
//!   * `throughput-export` — per-iteration cost of
//!     `LiteBox::export` on the same source. Reports the export
//!     wall + the resulting `.boxlite` archive bytes. Tests the
//!     box-archive tarball codepath, including disk image
//!     serialization.

use super::super::runner::{RunContext, Scenario, TeardownContext};
use super::common::{alpine_options, build_runtime};
use anyhow::{Context, Result};
use async_trait::async_trait;
use boxlite::runtime::options::{CloneOptions, ExportOptions};
use boxlite::{BoxCommand, LiteBox};
use futures::StreamExt;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Instant;
use tempfile::TempDir;

/// Pre-stage 64 MiB of dense data in /tmp inside the box so the
/// clone / export has to actually move bytes (cloning a sparse
/// COW with zero allocated blocks would be a useless metric).
async fn prepare_source(live: &LiteBox) -> Result<()> {
    let cmd = BoxCommand::new("dd").args([
        "if=/dev/zero",
        "of=/tmp/bench-clone-src",
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

// ─── latency-clone ─────────────────────────────────────────────────

pub struct LatencyClone {
    home: Option<TempDir>,
    prepared: bool,
    source_id: Option<String>,
}

impl LatencyClone {
    pub fn new() -> Self {
        Self {
            home: None,
            prepared: false,
            source_id: None,
        }
    }
}

#[async_trait]
impl Scenario for LatencyClone {
    fn name(&self) -> &str {
        "latency-clone"
    }

    async fn run_once(&mut self, ctx: &RunContext) -> Result<BTreeMap<String, f64>> {
        if self.home.is_none() {
            self.home = Some(TempDir::new().context("mkdir clone home")?);
        }
        let home_path = self
            .home
            .as_ref()
            .expect("just initialized")
            .path()
            .to_path_buf();
        let rt = build_runtime(ctx.global, home_path)?;

        // One-time source box prep: create, stage 64 MiB, stop
        // (clone requires a stopped source box).
        if !self.prepared {
            let mut opts = alpine_options();
            opts.auto_remove = false;
            let src = rt.create(opts, None).await.context("rt.create source")?;
            let src_id = src.id().to_string();
            src.start().await.context("source.start")?;
            prepare_source(&src).await?;
            src.stop().await.context("source.stop")?;
            self.source_id = Some(src_id);
            self.prepared = true;
        }
        let source_id = self.source_id.as_ref().expect("prepared").clone();
        let src = rt
            .get(&source_id)
            .await
            .context("re-get source box")?
            .context("source box vanished")?;

        let start = Instant::now();
        let clone = src
            .clone_box(CloneOptions::default(), None)
            .await
            .context("clone_box")?;
        let clone_ms = start.elapsed().as_secs_f64() * 1000.0;
        let clone_id = clone.id().to_string();

        // Remove the clone — accumulating would balloon disk.
        let _ = rt.remove(&clone_id, true).await;

        let mut metrics = BTreeMap::new();
        metrics.insert("clone_ms".into(), clone_ms);
        Ok(metrics)
    }

    async fn teardown(&mut self, ctx: &TeardownContext<'_>) -> Result<()> {
        let (Some(home), Some(src_id)) = (self.home.as_ref(), self.source_id.as_ref()) else {
            return Ok(());
        };
        let rt = build_runtime(ctx.global, home.path().to_path_buf())?;
        // Best-effort: source box was created with auto_remove=false
        // so it survives stop. Force-remove cascades any leftover
        // snapshots/clones still rooted on it.
        let _ = rt.remove(src_id, true).await;
        Ok(())
    }
}

// ─── throughput-export ─────────────────────────────────────────────

pub struct ThroughputExport {
    home: Option<TempDir>,
    prepared: bool,
    source_id: Option<String>,
    /// Scratch dir for export tarballs; same-home so it shares a
    /// filesystem with the source box's disk (no cross-mount copy
    /// cost in the export path).
    export_scratch: Option<TempDir>,
}

impl ThroughputExport {
    pub fn new() -> Self {
        Self {
            home: None,
            prepared: false,
            source_id: None,
            export_scratch: None,
        }
    }
}

#[async_trait]
impl Scenario for ThroughputExport {
    fn name(&self) -> &str {
        "throughput-export"
    }

    async fn run_once(&mut self, ctx: &RunContext) -> Result<BTreeMap<String, f64>> {
        if self.home.is_none() {
            self.home = Some(TempDir::new().context("mkdir export home")?);
        }
        if self.export_scratch.is_none() {
            self.export_scratch = Some(TempDir::new().context("mkdir export scratch")?);
        }
        let home_path = self
            .home
            .as_ref()
            .expect("just initialized")
            .path()
            .to_path_buf();
        let rt = build_runtime(ctx.global, home_path)?;

        if !self.prepared {
            let mut opts = alpine_options();
            opts.auto_remove = false;
            let src = rt.create(opts, None).await.context("rt.create source")?;
            let src_id = src.id().to_string();
            src.start().await.context("source.start")?;
            prepare_source(&src).await?;
            src.stop().await.context("source.stop")?;
            self.source_id = Some(src_id);
            self.prepared = true;
        }
        let source_id = self.source_id.as_ref().expect("prepared").clone();
        let src = rt
            .get(&source_id)
            .await
            .context("re-get source box")?
            .context("source box vanished")?;

        let dest_path = PathBuf::from(self.export_scratch.as_ref().expect("scratch").path())
            .join(format!("export-{}.boxlite", std::process::id()));

        let start = Instant::now();
        let _archive = src
            .export(ExportOptions::default(), &dest_path)
            .await
            .context("export")?;
        let export_ms = start.elapsed().as_secs_f64() * 1000.0;

        let archive_size = std::fs::metadata(&dest_path)
            .ok()
            .map(|m| m.len())
            .unwrap_or(0);

        // Clean up the archive so subsequent iterations start
        // with a fresh scratch dir (otherwise we accumulate).
        let _ = std::fs::remove_file(&dest_path);

        let mut metrics = BTreeMap::new();
        metrics.insert("export_ms".into(), export_ms);
        metrics.insert("export_bytes".into(), archive_size as f64);
        if export_ms > 0.0 {
            let mib = archive_size as f64 / (1024.0 * 1024.0);
            metrics.insert("export_mb_per_sec".into(), mib / (export_ms / 1000.0));
        }
        Ok(metrics)
    }

    async fn teardown(&mut self, ctx: &TeardownContext<'_>) -> Result<()> {
        let (Some(home), Some(src_id)) = (self.home.as_ref(), self.source_id.as_ref()) else {
            return Ok(());
        };
        let rt = build_runtime(ctx.global, home.path().to_path_buf())?;
        let _ = rt.remove(src_id, true).await;
        Ok(())
    }
}

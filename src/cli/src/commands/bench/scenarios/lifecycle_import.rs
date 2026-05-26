//! `BoxliteRuntime::import_box` throughput.
//!
//! Counterpart to `throughput-export` in `lifecycle.rs`. The
//! cluster-migration story is "export here, ship the .boxlite, import
//! there" — that round-trip is only as fast as the slower side. This
//! scenario measures the import half: tarball deserialization + disk
//! image reconstitution + DB row materialize.
//!
//! Setup: one-time, the scenario creates a source box, writes 64 MiB
//! of dense data, and exports to a scratch file. Each iteration calls
//! `rt.import_box(BoxArchive::new(path), None)`, times it, then
//! force-removes the imported box so disk doesn't grow unbounded.

use super::super::runner::{RunContext, Scenario};
use super::common::{alpine_options, build_runtime};
use anyhow::{Context, Result};
use async_trait::async_trait;
use boxlite::runtime::options::{BoxArchive, ExportOptions};
use boxlite::{BoxCommand, LiteBox};
use futures::StreamExt;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Instant;
use tempfile::TempDir;

const STAGE_MIB: u32 = 64;

async fn prepare_source(live: &LiteBox) -> Result<()> {
    let cmd = BoxCommand::new("dd").args([
        "if=/dev/zero",
        "of=/tmp/bench-import-src",
        "bs=1M",
        &format!("count={STAGE_MIB}"),
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

pub struct ThroughputImport {
    home: Option<TempDir>,
    export_scratch: Option<TempDir>,
    archive_path: Option<PathBuf>,
    archive_bytes: u64,
}

impl ThroughputImport {
    pub fn new() -> Self {
        Self {
            home: None,
            export_scratch: None,
            archive_path: None,
            archive_bytes: 0,
        }
    }
}

#[async_trait]
impl Scenario for ThroughputImport {
    fn name(&self) -> &str {
        "throughput-import"
    }

    async fn run_once(&mut self, ctx: &RunContext) -> Result<BTreeMap<String, f64>> {
        if self.home.is_none() {
            self.home = Some(TempDir::new().context("mkdir import home")?);
        }
        if self.export_scratch.is_none() {
            self.export_scratch = Some(TempDir::new().context("mkdir import scratch")?);
        }
        let home_path = self
            .home
            .as_ref()
            .expect("just initialized")
            .path()
            .to_path_buf();
        let rt = build_runtime(ctx.global, home_path)?;

        // One-time: stage a source box, write 64 MiB, export to
        // scratch. Reuse the archive across iterations so the
        // measurement is import-only, not export+import.
        if self.archive_path.is_none() {
            let mut opts = alpine_options();
            opts.auto_remove = false;
            let src = rt.create(opts, None).await.context("rt.create source")?;
            let src_id = src.id().to_string();
            src.start().await.context("source.start")?;
            prepare_source(&src).await?;
            src.stop().await.context("source.stop")?;

            let dest = self
                .export_scratch
                .as_ref()
                .expect("scratch")
                .path()
                .join("import-source.boxlite");
            let _archive = src
                .export(ExportOptions::default(), &dest)
                .await
                .context("export source")?;
            self.archive_bytes = std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
            self.archive_path = Some(dest);

            // Remove the source box now that the archive is on
            // disk — keeps the home small while iterations run.
            let _ = rt.remove(&src_id, true).await;
        }
        let archive_path = self.archive_path.as_ref().expect("archived").clone();
        let archive_bytes = self.archive_bytes;

        let archive = BoxArchive::new(archive_path);
        let start = Instant::now();
        let imported = rt.import_box(archive, None).await.context("import_box")?;
        let import_ms = start.elapsed().as_secs_f64() * 1000.0;
        let imported_id = imported.id().to_string();
        drop(imported);
        let _ = rt.remove(&imported_id, true).await;

        let mut metrics = BTreeMap::new();
        metrics.insert("import_ms".into(), import_ms);
        metrics.insert("import_bytes".into(), archive_bytes as f64);
        if import_ms > 0.0 {
            let mib = archive_bytes as f64 / (1024.0 * 1024.0);
            metrics.insert("import_mb_per_sec".into(), mib / (import_ms / 1000.0));
        }
        Ok(metrics)
    }
}

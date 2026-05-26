//! Snapshot create + restore latency.
//!
//! `SnapshotHandle::{create, restore}` are point-in-time disk-state
//! ops. Common workflow: snapshot a clean baseline, do work, restore
//! to the baseline. Both halves are on the hot path; this scenario
//! times each.
//!
//! Per iteration:
//!   1. Pre-stage 64 MiB of data on the source box (one-time per
//!      scenario instance).
//!   2. Create snapshot `bench-snap-<iter>`, time it.
//!   3. Restore from the snapshot, time it.
//!
//! Note: we deliberately do NOT remove the snapshot here. After a
//! restore, the current disk depends on the snapshot, so removing
//! it fails with `Cannot remove snapshot: current disk depends on
//! this snapshot`. The snapshot CRUD churn rate is measured by
//! `stability-snapshot-loop` (no restore between create/remove,
//! so the dependency invariant doesn't kick in).
//!
//! Reports:
//!   * `snapshot_create_ms` — `.create()` wall.
//!   * `snapshot_restore_ms` — `.restore()` wall.

use super::super::runner::{RunContext, Scenario, TeardownContext};
use super::common::{alpine_options, build_runtime};
use anyhow::{Context, Result};
use async_trait::async_trait;
use boxlite::BoxCommand;
use boxlite::runtime::options::SnapshotOptions;
use futures::StreamExt;
use std::collections::BTreeMap;
use std::time::Instant;
use tempfile::TempDir;

pub struct Snapshot {
    home: Option<TempDir>,
    source_id: Option<String>,
    staged: bool,
}

impl Snapshot {
    pub fn new() -> Self {
        Self {
            home: None,
            source_id: None,
            staged: false,
        }
    }
}

#[async_trait]
impl Scenario for Snapshot {
    fn name(&self) -> &str {
        "latency-snapshot"
    }

    async fn run_once(&mut self, ctx: &RunContext) -> Result<BTreeMap<String, f64>> {
        if self.home.is_none() {
            self.home = Some(TempDir::new().context("mkdir snapshot home")?);
        }
        let home_path = self
            .home
            .as_ref()
            .expect("just initialized")
            .path()
            .to_path_buf();
        let rt = build_runtime(ctx.global, home_path)?;

        // Source box prep — once per scenario instance.
        if !self.staged {
            let mut opts = alpine_options();
            opts.auto_remove = false;
            let src = rt.create(opts, None).await.context("rt.create source")?;
            let src_id = src.id().to_string();
            src.start().await.context("source.start")?;

            let dd = BoxCommand::new("dd").args([
                "if=/dev/zero",
                "of=/tmp/baseline",
                "bs=1M",
                "count=64",
                "conv=fsync",
            ]);
            let mut exec = src.exec(dd).await.context("dd stage")?;
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
            src.stop().await.context("source.stop")?;
            self.source_id = Some(src_id);
            self.staged = true;
        }

        let source_id = self.source_id.as_ref().expect("staged").clone();
        let src = rt
            .get(&source_id)
            .await
            .context("re-get source box")?
            .context("source vanished")?;

        // Unique snapshot name per iteration (just in case any
        // remove() races; cheap insurance).
        let snap_name = format!(
            "bench-snap-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );

        let snapshots = src.snapshots();
        let t0 = Instant::now();
        let _info = snapshots
            .create(SnapshotOptions::default(), &snap_name)
            .await
            .with_context(|| format!("snapshots.create({snap_name})"))?;
        let create_ms = t0.elapsed().as_secs_f64() * 1000.0;

        let t1 = Instant::now();
        snapshots
            .restore(&snap_name)
            .await
            .with_context(|| format!("snapshots.restore({snap_name})"))?;
        let restore_ms = t1.elapsed().as_secs_f64() * 1000.0;

        let mut metrics = BTreeMap::new();
        metrics.insert("snapshot_create_ms".into(), create_ms);
        metrics.insert("snapshot_restore_ms".into(), restore_ms);
        Ok(metrics)
    }

    async fn teardown(&mut self, ctx: &TeardownContext<'_>) -> Result<()> {
        let (Some(home), Some(src_id)) = (self.home.as_ref(), self.source_id.as_ref()) else {
            return Ok(());
        };
        let rt = build_runtime(ctx.global, home.path().to_path_buf())?;
        // Force-remove cascades the accumulated snapshots (created
        // per iteration; left behind because the qcow2 dep
        // invariant prevents removing them while the source's
        // current disk depends on them).
        let _ = rt.remove(src_id, true).await;
        Ok(())
    }
}

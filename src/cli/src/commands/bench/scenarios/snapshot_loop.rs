//! Snapshot create churn under sustained load.
//!
//! `latency-snapshot` measures one create+restore round-trip. This
//! scenario runs N=20 sequential creates on the same source box and
//! reports per-create latency + cumulative disk growth — catches
//! create-time accumulators (leaked DB rows, growing
//! `boxes/<id>/snapshots/` dir, qcow2 chain depth costs).
//!
//! `remove` is deliberately omitted: in this qcow2 chain model,
//! creating snapshot N makes the current overlay depend on N, so
//! `remove(N)` fails ("current disk depends on this snapshot") until
//! a later N+1 is created. Mixing creates and removes in the loop
//! would couple the two latencies and obscure what each costs alone.
//!
//! Reports per-cycle aggregates over the 20 iterations:
//!   * `snap_loop_cycles` — N (always 20).
//!   * `snap_loop_create_mean_ms`, `snap_loop_create_max_ms` —
//!     primary signal. Latency growth across the loop (max > 2×
//!     mean) suggests qcow2 chain-depth or store-lookup scaling.
//!   * `snap_loop_cow_growth_bytes` — change in source box's COW
//!     bytes from start to end of loop. A leaking create path
//!     would show monotone growth past the small per-snap header
//!     cost.

use super::super::runner::{RunContext, Scenario, TeardownContext};
use super::common::{alpine_options, build_runtime};
use anyhow::{Context, Result};
use async_trait::async_trait;
use boxlite::BoxCommand;
use boxlite::runtime::options::SnapshotOptions;
use futures::StreamExt;
use std::collections::BTreeMap;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::Instant;
use tempfile::TempDir;

const CYCLES: usize = 20;

/// Allocated bytes (st_blocks * 512) of the box's COW disk.qcow2.
/// Mirrors `resource_density::cow_disk_size` — kept here too because
/// snapshot leaks would specifically show up as overlay-bytes growing
/// even after the snap is removed.
fn cow_disk_size(home: &Path, box_id: &str) -> Option<u64> {
    let path = home
        .join("boxes")
        .join(box_id)
        .join("disks")
        .join("disk.qcow2");
    std::fs::metadata(&path).ok().map(|m| m.blocks() * 512)
}

pub struct SnapshotLoop {
    home: Option<TempDir>,
    source_id: Option<String>,
    staged: bool,
}

impl SnapshotLoop {
    pub fn new() -> Self {
        Self {
            home: None,
            source_id: None,
            staged: false,
        }
    }
}

#[async_trait]
impl Scenario for SnapshotLoop {
    fn name(&self) -> &str {
        "stability-snapshot-loop"
    }

    async fn run_once(&mut self, ctx: &RunContext) -> Result<BTreeMap<String, f64>> {
        if self.home.is_none() {
            self.home = Some(TempDir::new().context("mkdir snap-loop home")?);
        }
        let home_path = self
            .home
            .as_ref()
            .expect("just initialized")
            .path()
            .to_path_buf();
        let rt = build_runtime(ctx.global, home_path)?;

        if !self.staged {
            let mut opts = alpine_options();
            opts.auto_remove = false;
            let src = rt.create(opts, None).await.context("rt.create source")?;
            let src_id = src.id().to_string();
            src.start().await.context("source.start")?;
            let dd = BoxCommand::new("dd").args([
                "if=/dev/zero",
                "of=/tmp/snap-baseline",
                "bs=1M",
                "count=32",
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
            .context("re-get source")?
            .context("source vanished")?;

        let snapshots = src.snapshots();

        let cow_before =
            cow_disk_size(self.home.as_ref().expect("home").path(), &source_id).unwrap_or(0);

        let mut create_times = Vec::with_capacity(CYCLES);

        for i in 0..CYCLES {
            let snap_name = format!(
                "bench-snap-loop-{i}-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            );
            let t0 = Instant::now();
            let _ = snapshots
                .create(SnapshotOptions::default(), &snap_name)
                .await
                .with_context(|| format!("snapshots.create({snap_name})"))?;
            create_times.push(t0.elapsed().as_secs_f64() * 1000.0);
        }

        let cow_after =
            cow_disk_size(self.home.as_ref().expect("home").path(), &source_id).unwrap_or(0);
        // src kept alive so the COW path stays mapped while we
        // read st_blocks back.
        drop(src);

        fn mean(v: &[f64]) -> f64 {
            v.iter().copied().sum::<f64>() / v.len() as f64
        }
        fn max(v: &[f64]) -> f64 {
            v.iter().copied().fold(f64::NEG_INFINITY, f64::max)
        }

        let mut metrics = BTreeMap::new();
        metrics.insert("snap_loop_cycles".into(), CYCLES as f64);
        metrics.insert("snap_loop_create_mean_ms".into(), mean(&create_times));
        metrics.insert("snap_loop_create_max_ms".into(), max(&create_times));
        metrics.insert(
            "snap_loop_cow_growth_bytes".into(),
            cow_after as f64 - cow_before as f64,
        );
        Ok(metrics)
    }

    async fn teardown(&mut self, ctx: &TeardownContext<'_>) -> Result<()> {
        let (Some(home), Some(src_id)) = (self.home.as_ref(), self.source_id.as_ref()) else {
            return Ok(());
        };
        let rt = build_runtime(ctx.global, home.path().to_path_buf())?;
        // 20 snapshots accumulate per iter; force-remove cascades.
        let _ = rt.remove(src_id, true).await;
        Ok(())
    }
}

//! N consecutive stop+start cycles on the SAME box —
//! distinct from `stability-churn` (which creates a new box each
//! cycle). Tests the warm-restart path: the box's persisted state
//! is reused, only the libkrun VM + gvproxy are torn down and
//! respawned.
//!
//! Regression class this catches:
//!   * COW disk that grows on each restart (a write that doesn't
//!     get clamped after stop);
//!   * boxlite DB rows that accumulate per restart;
//!   * `BoxState::port_mappings` accumulation (regression guard
//!     for the 568 fix's clear-on-stop semantic);
//!   * libkrun-shim binary file descriptors that aren't reused.

use super::super::runner::{RunContext, Scenario};
use super::common::{BoxGuard, alpine_options, build_runtime};
use anyhow::{Context, Result};
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Instant;
use tempfile::TempDir;

const N: usize = 20;

pub struct RestartLoop {
    home: Option<TempDir>,
}

impl RestartLoop {
    pub fn new() -> Self {
        Self { home: None }
    }
}

#[async_trait]
impl Scenario for RestartLoop {
    fn name(&self) -> &str {
        "stability-restart-loop"
    }

    async fn run_once(&mut self, ctx: &RunContext) -> Result<BTreeMap<String, f64>> {
        if self.home.is_none() {
            self.home = Some(TempDir::new().context("mkdir restart-loop home")?);
        }
        let home_path = self
            .home
            .as_ref()
            .expect("just initialized")
            .path()
            .to_path_buf();
        let rt = build_runtime(ctx.global, home_path.clone())?;

        // Create-once. We do N stop+start cycles on this same
        // box; `auto_remove=true` would delete it on the first
        // stop, so override.
        let mut opts = alpine_options();
        opts.auto_remove = false;
        let live = rt.create(opts, None).await.context("rt.create(alpine)")?;
        let box_id = live.id().to_string();
        let mut guard = BoxGuard::new(&rt, box_id.clone());
        live.start().await.context("box.start() initial")?;

        let cow_before = cow_disk_size(&home_path, &box_id).unwrap_or(0);
        let fd_before = host_fd_count();

        // After `stop`, the LiteBox handle is invalidated — boxlite
        // requires `runtime.get(id)` to obtain a fresh handle before
        // the next `start`. We re-fetch every cycle so the loop
        // actually exercises the warm-restart path instead of
        // bailing on cycle 0 with "Handle invalidated after stop()".
        let mut handle = live;
        let mut per_cycle = Vec::with_capacity(N);
        for i in 0..N {
            let start = Instant::now();
            handle
                .stop()
                .await
                .with_context(|| format!("cycle {i}: stop"))?;
            handle = rt
                .get(&box_id)
                .await
                .with_context(|| format!("cycle {i}: re-get post-stop"))?
                .with_context(|| format!("cycle {i}: box vanished after stop"))?;
            handle
                .start()
                .await
                .with_context(|| format!("cycle {i}: start"))?;
            per_cycle.push(start.elapsed().as_secs_f64() * 1000.0);
        }

        let cow_after = cow_disk_size(&home_path, &box_id).unwrap_or(0);
        let fd_after = host_fd_count();

        // Final teardown — explicit since auto_remove was false.
        handle.stop().await.context("final stop")?;
        let _ = rt.remove(&box_id, true).await;
        guard.disarm();

        let mut metrics = BTreeMap::new();
        metrics.insert("restart_cycles_count".into(), N as f64);
        let mean = per_cycle.iter().copied().sum::<f64>() / per_cycle.len() as f64;
        let max = per_cycle.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        metrics.insert("restart_cycle_mean_ms".into(), mean);
        metrics.insert("restart_cycle_max_ms".into(), max);
        metrics.insert(
            "restart_cow_growth_bytes".into(),
            cow_after as f64 - cow_before as f64,
        );
        if let (Some(before), Some(after)) = (fd_before, fd_after) {
            metrics.insert(
                "restart_fd_delta_count".into(),
                after as f64 - before as f64,
            );
        }
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

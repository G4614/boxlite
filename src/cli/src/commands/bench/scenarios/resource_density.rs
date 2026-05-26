//! Steady-state multi-box resource footprint.
//!
//! Distinct from `density-parallel-10` (which measures concurrent
//! SPAWN latency under contention) — this scenario measures
//! coexistence cost: how much RSS / COW / fd does the host carry
//! when N idle boxes are alive at the same time. Answers the
//! "can I run N concurrent boxes on this host" question that
//! latency benchmarks don't address.
//!
//! Per iteration:
//!   1. Probe host fd count.
//!   2. Spawn N alpine boxes through one shared runtime (sequential
//!      starts — concurrent SPAWN is the other scenario; here we
//!      care about the steady-state total, not the spawn cost).
//!   3. Settle 3 s so every guest agent finishes its post-boot
//!      initialization.
//!   4. Sample `BoxMetrics` for each of the N boxes; sum RSS.
//!   5. Stat each box's COW overlay on disk; sum bytes.
//!   6. Probe host fd count again.
//!   7. Tear down all N boxes.
//!
//! Reports (all "lower-is-better" by default — no _per_sec or _rps
//! suffix triggers the throughput flip):
//!   * `density_box_count` — N as a const captured into the report
//!     so a future change from N=10 to N=20 doesn't silently break
//!     the comparator.
//!   * `density_total_rss_bytes` — sum across the N boxes.
//!   * `density_per_box_rss_mean_bytes` — convenience derived
//!     (total / N).
//!   * `density_total_cow_bytes` — sum of `st_blocks * 512` for each
//!     `<home>/boxes/<id>/disks/disk.qcow2`.
//!   * `density_host_fd_delta_count` — increase in `/proc/self/fd`
//!     count from before-spawn to all-settled.

use super::super::runner::{RunContext, Scenario};
use super::common::{BoxGuard, alpine_options, build_runtime};
use anyhow::{Context, Result};
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::Duration;
use tempfile::TempDir;

const N: usize = 10;
const SETTLE: Duration = Duration::from_secs(3);

pub struct DensityIdle {
    home: Option<TempDir>,
}

impl DensityIdle {
    pub fn new() -> Self {
        Self { home: None }
    }
}

#[async_trait]
impl Scenario for DensityIdle {
    fn name(&self) -> &str {
        "resource-density-10-idle"
    }

    async fn run_once(&mut self, ctx: &RunContext) -> Result<BTreeMap<String, f64>> {
        if self.home.is_none() {
            self.home = Some(TempDir::new().context("mkdir density-idle home")?);
        }
        let home_path = self
            .home
            .as_ref()
            .expect("just initialized")
            .path()
            .to_path_buf();
        let rt = build_runtime(ctx.global, home_path.clone())?;

        let fd_before = host_fd_count();

        // Sequential starts (NOT tokio::spawn fan-out). The point
        // is steady-state, not spawn contention. Concurrent spawn
        // is `density-parallel-10`.
        let mut boxes = Vec::with_capacity(N);
        for i in 0..N {
            let live = rt
                .create(alpine_options(), None)
                .await
                .with_context(|| format!("rt.create alpine #{i}"))?;
            let id = live.id().to_string();
            let guard = BoxGuard::new(&rt, id.clone());
            live.start()
                .await
                .with_context(|| format!("box.start alpine #{i}"))?;
            boxes.push((live, id, guard));
        }

        tokio::time::sleep(SETTLE).await;

        // Per-box snapshots.
        let mut total_rss: u64 = 0;
        let mut rss_count: usize = 0;
        let mut total_cow: u64 = 0;
        for (live, id, _guard) in &boxes {
            let snap = live
                .metrics()
                .await
                .with_context(|| format!("BoxMetrics for {id}"))?;
            if let Some(r) = snap.memory_bytes() {
                total_rss += r;
                rss_count += 1;
            }
            if let Some(c) = cow_disk_size(&home_path, id) {
                total_cow += c;
            }
        }

        let fd_after = host_fd_count();

        // Tear down. `BoxGuard` holds `&BoxliteRuntime`, which
        // can't satisfy tokio::spawn's `'static`, so we serialize
        // the stops. At this point all metrics are already
        // collected, so serial vs concurrent teardown doesn't
        // affect the numbers — only iteration wall.
        for (live, _id, mut guard) in boxes {
            let _ = live.stop().await;
            guard.disarm();
        }

        let mut metrics = BTreeMap::new();
        metrics.insert("density_box_count".into(), N as f64);
        if rss_count > 0 {
            metrics.insert("density_total_rss_bytes".into(), total_rss as f64);
            metrics.insert(
                "density_per_box_rss_mean_bytes".into(),
                total_rss as f64 / rss_count as f64,
            );
        }
        if total_cow > 0 {
            metrics.insert("density_total_cow_bytes".into(), total_cow as f64);
        }
        if let (Some(before), Some(after)) = (fd_before, fd_after) {
            metrics.insert(
                "density_host_fd_delta_count".into(),
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
    let path = home
        .join("boxes")
        .join(box_id)
        .join("disks")
        .join("disk.qcow2");
    std::fs::metadata(&path).ok().map(|m| m.blocks() * 512)
}

//! Resource scenarios: how much does ONE idle box cost.
//!
//! Single scenario for Phase 2: `resource-idle`. Each iteration
//! creates a fresh alpine box, lets it settle for a few seconds (so
//! the libkrun VM finishes booting + the agent's first GC cycle
//! settles), then samples RSS / COW footprint / CPU.
//!
//! Metrics produced:
//!   * `rss_bytes` — `BoxMetrics::memory_bytes()`, the libkrun VM
//!     process's resident set size from `/proc/<pid>/status` (via the
//!     box backend). Headline footprint number.
//!   * `cow_bytes` — actual on-disk size of the container's COW
//!     overlay file (`<home>/boxes/<id>/disks/disk.qcow2`). Tracks
//!     "disk this single box materialized on top of the shared base
//!     image" — different from the COW's virtual size.
//!   * `cpu_idle_pct` — `BoxMetrics::cpu_percent()`, the libkrun
//!     VM's CPU consumption at the sample instant. For an idle alpine
//!     box this should be near 0; a regression here would say "the
//!     guest agent is burning CPU when it should be parked on its
//!     vsock listener."
//!
//! Home is shared across iterations (warm cache) so the numbers reflect
//! steady-state per-box cost rather than first-time bootstrap noise.
//! That mirrors the `latency-warm-start` model and keeps the per-
//! iteration wall under 5 s.

use super::super::runner::{RunContext, Scenario};
use super::common::{BoxGuard, alpine_options, build_runtime};
use anyhow::{Context, Result};
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;
use tempfile::TempDir;

/// How long to wait between `start` and the resource snapshot. The
/// guest agent does a small amount of work right after boot (mount
/// virtiofs, configure network, write the ready signal) — sampling
/// before that work finishes inflates `cpu_idle_pct` and the
/// post-boot transient page-cache activity inflates `rss_bytes`.
/// Three seconds is enough for the guest to quiesce on every machine
/// I've tested; pushing it lower is a future optimization gated on a
/// scheduler/agent improvement that we'd see here as a regression.
const SETTLE: Duration = Duration::from_secs(3);

/// `resource-idle` — single-box idle footprint snapshot.
pub struct ResourceIdle {
    /// Shared `--home` across iterations so we're measuring steady-
    /// state cost, not first-time pull + bootstrap.
    home: Option<TempDir>,
}

impl ResourceIdle {
    pub fn new() -> Self {
        Self { home: None }
    }
}

#[async_trait]
impl Scenario for ResourceIdle {
    fn name(&self) -> &str {
        "resource-idle"
    }

    async fn run_once(&mut self, ctx: &RunContext) -> Result<BTreeMap<String, f64>> {
        if self.home.is_none() {
            self.home = Some(TempDir::new().context("mkdir resource-idle home")?);
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
        tokio::time::sleep(SETTLE).await;

        let mut metrics = BTreeMap::new();
        let snapshot = live.metrics().await.context("snapshot BoxMetrics")?;

        if let Some(rss) = snapshot.memory_bytes() {
            metrics.insert("rss_bytes".into(), rss as f64);
        }
        if let Some(cpu) = snapshot.cpu_percent() {
            metrics.insert("cpu_idle_pct".into(), cpu as f64);
        }
        if let Some(size) = cow_disk_size(&home_path, &box_id) {
            metrics.insert("cow_bytes".into(), size as f64);
        }

        live.stop().await.context("box.stop()")?;
        guard.disarm();
        Ok(metrics)
    }
}

/// Read the on-disk size of the box's container COW overlay
/// (`<home>/boxes/<id>/disks/disk.qcow2`). qcow2 is sparse — the
/// `metadata().len()` here is the *virtual* size, not the materialized
/// blocks; for materialized blocks we'd need `stat().st_blocks * 512`.
/// We report `st_blocks * 512` (the number of bytes the COW actually
/// occupies) since "how much disk this box's writes cost" is what
/// users want to bench. Returns `None` if the file is missing — the
/// scenario then just omits the metric from the sample.
fn cow_disk_size(home: &Path, box_id: &str) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    let path = home
        .join("boxes")
        .join(box_id)
        .join("disks")
        .join("disk.qcow2");
    std::fs::metadata(&path).ok().map(|m| m.blocks() * 512)
}

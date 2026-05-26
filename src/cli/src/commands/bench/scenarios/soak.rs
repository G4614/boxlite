//! Long-running soak scenario.
//!
//! `stability-soak` — keeps a single alpine box alive for
//! `BOXLITE_BENCH_SOAK_SECS` seconds (default `30`), sampling RSS /
//! COW disk size / host fd count every `SAMPLE_INTERVAL`. Reports
//! the first→last deltas + the max RSS observed.
//!
//! Why this isn't just a longer `stability-churn`: churn measures
//! the *create+stop* path's per-cycle leak surface. Soak measures
//! the *steady-state* leak surface — the guest agent, the host shim,
//! the gvproxy goroutines all sitting there doing nothing for N
//! seconds. A regression where, e.g., the gvproxy event loop
//! allocates one byte per packet and never frees it would NOT show
//! up in churn (no traffic) but DOES show up here.
//!
//! Why an env var (`BOXLITE_BENCH_SOAK_SECS`) instead of a CLI flag:
//! the bench harness has one canonical `RunArgs` that's shared by
//! every scenario; adding a scenario-specific flag (`--soak-secs N`)
//! to `RunArgs` would clutter every other scenario's `--help`
//! output. An env var keeps the contract scenario-local. When we
//! grow a generic `--scenario-arg KEY=VAL` knob this scenario will
//! be the first user.
//!
//! Defaults:
//!   * 30 s soak — long enough to surface a steady leak rate, short
//!     enough that a single bench iteration completes in under a
//!     minute. Bump via env var for a real soak.
//!   * 2 s sample interval — 15 samples in a 30 s run, enough to
//!     spot a non-linear curve if growth isn't monotonic.
//!
//! Reports (all numeric; aggregator works the same as anywhere):
//!   * `soak_secs` — actual elapsed duration.
//!   * `rss_growth_bytes` — last sample's RSS minus first sample's.
//!     Steady-state leak indicator.
//!   * `rss_max_bytes` — peak RSS observed during the soak.
//!   * `cow_growth_bytes` — last sample's COW size minus first.
//!     Catches "writes-on-idle" regressions inside the agent.
//!   * `fd_growth_count` — host-side `/proc/self/fd` delta.

use super::super::runner::{RunContext, Scenario};
use super::common::{BoxGuard, alpine_options, build_runtime};
use anyhow::{Context, Result};
use async_trait::async_trait;
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

pub struct Soak {
    home: Option<TempDir>,
}

impl Soak {
    pub fn new() -> Self {
        Self { home: None }
    }
}

#[async_trait]
impl Scenario for Soak {
    fn name(&self) -> &str {
        "stability-soak"
    }

    async fn run_once(&mut self, ctx: &RunContext) -> Result<BTreeMap<String, f64>> {
        let soak_secs = configured_soak_secs();
        let deadline = Instant::now() + Duration::from_secs(soak_secs);

        if self.home.is_none() {
            self.home = Some(TempDir::new().context("mkdir soak home")?);
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

        let fd_first = host_fd_count();
        let mut rss_samples: Vec<u64> = Vec::new();
        let mut cow_samples: Vec<u64> = Vec::new();

        // First sample immediately after start; subsequent samples
        // every SAMPLE_INTERVAL until the soak deadline.
        loop {
            let snapshot = live.metrics().await.context("snapshot BoxMetrics")?;
            if let Some(rss) = snapshot.memory_bytes() {
                rss_samples.push(rss);
            }
            if let Some(cow) = cow_disk_size(&home_path, &box_id) {
                cow_samples.push(cow);
            }
            if Instant::now() >= deadline {
                break;
            }
            // Sleep until next sample, but not past the deadline.
            let remaining = deadline.saturating_duration_since(Instant::now());
            let sleep_for = remaining.min(SAMPLE_INTERVAL);
            if sleep_for.is_zero() {
                break;
            }
            tokio::time::sleep(sleep_for).await;
        }

        let fd_last = host_fd_count();

        live.stop().await.context("box.stop()")?;
        guard.disarm();

        let mut metrics = BTreeMap::new();
        metrics.insert("soak_secs".into(), soak_secs as f64);

        if let (Some(&first), Some(&last)) = (rss_samples.first(), rss_samples.last()) {
            metrics.insert("rss_growth_bytes".into(), last as f64 - first as f64);
        }
        if let Some(&peak) = rss_samples.iter().max() {
            metrics.insert("rss_max_bytes".into(), peak as f64);
        }
        if let (Some(&first), Some(&last)) = (cow_samples.first(), cow_samples.last()) {
            metrics.insert("cow_growth_bytes".into(), last as f64 - first as f64);
        }
        if let (Some(before), Some(after)) = (fd_first, fd_last) {
            metrics.insert("fd_growth_count".into(), after as f64 - before as f64);
        }

        Ok(metrics)
    }
}

fn host_fd_count() -> Option<usize> {
    let entries = std::fs::read_dir("/proc/self/fd").ok()?;
    Some(entries.count())
}

fn cow_disk_size(home: &Path, box_id: &str) -> Option<u64> {
    let path = PathBuf::from(home)
        .join("boxes")
        .join(box_id)
        .join("disks")
        .join("disk.qcow2");
    std::fs::metadata(&path).ok().map(|m| m.blocks() * 512)
}

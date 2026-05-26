//! Stability scenarios: does boxlite degrade over time?
//!
//! Phase 5 ships ONE scenario, `stability-churn`. The long-running
//! "soak" axis (one box alive for N hours, periodic RSS samples to
//! catch slow growth) is deliberately deferred: a 24 h soak doesn't
//! fit a normal PR review cycle, and the lower-bound signal — "after
//! 50 create+stop cycles, does the next one cost the same as the
//! first?" — catches the regression class that matters most
//! (per-cycle fd / temp-file / DB-row leaks) without needing
//! overnight runs.
//!
//! `stability-churn` — per iteration: do `CHURN_CYCLES` consecutive
//! create+start+stop cycles through one shared `--home`, then
//! sample:
//!   * `churn_cycles_count` — `CHURN_CYCLES` as a const captured
//!     into the report.
//!   * `cycle_mean_ms` — average time per cycle.
//!   * `cycle_max_ms` — slowest single cycle in the run. A leak that
//!     scales with cycle count shows up here as `max ≈ N × mean`.
//!   * `fd_delta_count` — `/proc/self/fd` entry count delta from
//!     start to end of the iteration. Catches host-side fd leaks
//!     boxlite-cli (this process) creates per cycle. Won't catch
//!     leaks inside the libkrun VM itself — that's the soak's job.
//!
//! Warm-cache (shared `--home`) by construction: a cold churn would
//! be re-measuring `latency-cold-start` 50 times. Steady-state per-
//! cycle cost is what we care about; the first one is warmup-noise
//! and is exposed as `cycle_max_ms` if it's an outlier.
//!
//! Future work:
//!   * `stability-soak` (gated behind `--soak-hours N`) — one box
//!     alive for hours, periodic RSS+fd snapshots. Mostly time +
//!     plotting; the harness already supports it via run-loop pacing.

use super::super::runner::{RunContext, Scenario};
use super::common::{BoxGuard, alpine_options, build_runtime};
use anyhow::{Context, Result};
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::time::Instant;
use tempfile::TempDir;

/// How many create+start+stop cycles to run inside a single
/// iteration. Picked so that an iteration completes in roughly a
/// minute on a warm cache (50 × ~1s warm-start ≈ 50s on a 4-core
/// box). Higher N tightens the per-cycle mean confidence but
/// extends wall time linearly.
const CHURN_CYCLES: usize = 50;

pub struct Churn {
    /// Shared `--home` across iterations — first iteration warms
    /// the alpine cache; later iterations measure pure cycle cost.
    home: Option<TempDir>,
}

impl Churn {
    pub fn new() -> Self {
        Self { home: None }
    }
}

#[async_trait]
impl Scenario for Churn {
    fn name(&self) -> &str {
        "stability-churn"
    }

    async fn run_once(&mut self, ctx: &RunContext) -> Result<BTreeMap<String, f64>> {
        if self.home.is_none() {
            self.home = Some(TempDir::new().context("mkdir stability-churn home")?);
        }
        let home_path = self
            .home
            .as_ref()
            .expect("just initialized")
            .path()
            .to_path_buf();
        let rt = build_runtime(ctx.global, home_path)?;

        let fd_before = host_fd_count();

        let mut per_cycle = Vec::with_capacity(CHURN_CYCLES);
        for cycle in 0..CHURN_CYCLES {
            let start = Instant::now();
            let live = rt
                .create(alpine_options(), None)
                .await
                .with_context(|| format!("cycle {cycle}: rt.create"))?;
            let mut guard = BoxGuard::new(&rt, live.id().to_string());
            live.start()
                .await
                .with_context(|| format!("cycle {cycle}: box.start"))?;
            live.stop()
                .await
                .with_context(|| format!("cycle {cycle}: box.stop"))?;
            guard.disarm();
            per_cycle.push(start.elapsed().as_secs_f64() * 1000.0);
        }

        let fd_after = host_fd_count();

        let mut metrics = BTreeMap::new();
        metrics.insert("churn_cycles_count".into(), CHURN_CYCLES as f64);
        let mean = per_cycle.iter().copied().sum::<f64>() / per_cycle.len() as f64;
        let max = per_cycle.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        metrics.insert("cycle_mean_ms".into(), mean);
        metrics.insert("cycle_max_ms".into(), max);
        if let (Some(before), Some(after)) = (fd_before, fd_after) {
            // Signed because a successful run can release a few fds
            // it inherited from previous tasks; we still want to see
            // small negatives in the report rather than silently
            // clamping to 0.
            metrics.insert("fd_delta_count".into(), after as f64 - before as f64);
        }

        Ok(metrics)
    }
}

/// Count entries in `/proc/self/fd`. Returns `None` if the directory
/// can't be read (non-Linux, restrictive sandbox). The count
/// includes the `readdir` fd itself but it cancels out across
/// before/after.
fn host_fd_count() -> Option<usize> {
    let entries = std::fs::read_dir("/proc/self/fd").ok()?;
    Some(entries.count())
}

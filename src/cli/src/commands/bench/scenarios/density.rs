//! Density scenario: burst-spawn N boxes concurrently.
//!
//! `density-parallel-10` (N fixed for Phase 3) spawns 10 alpine boxes
//! through one shared `BoxliteRuntime` at once via `tokio::spawn`.
//! Measures:
//!
//!   * `wall_ms` (from the runner) — total elapsed time from "kick
//!     off the spawn fan-out" to "every box has reported Running."
//!     The N-1 boxes that started later wait on shared serialized
//!     init paths (image cache lock, base disk build, etc.), so the
//!     gap between this and the latency-warm-start single-box
//!     `wall_ms * N` is the contention surcharge.
//!   * `parallel_max_latency_ms` — the slowest individual box's
//!     create+start time inside the burst. Headline tail number.
//!   * `parallel_mean_latency_ms` — average across the N boxes.
//!   * `parallel_boxes_count` — N as a const, captured into the
//!     report so cross-scenario diffs don't silently compare a
//!     parallel-5 baseline against a parallel-10 current.
//!
//! Shared `--home` across iterations (warm cache). The first
//! iteration pays the alpine pull + base disk build; subsequent
//! iterations measure pure concurrency contention.
//!
//! Cleanup: all N boxes are torn down concurrently after the metric
//! snapshot. A `BoxGuard` is held for each so a panic mid-iteration
//! doesn't leak the whole fleet.

use super::super::runner::{RunContext, Scenario};
use super::common::{BoxGuard, alpine_options, build_runtime};
use anyhow::{Context, Result};
use async_trait::async_trait;
use boxlite::BoxliteRuntime;
use std::collections::BTreeMap;
use std::time::Instant;
use tempfile::TempDir;

/// Default burst size. Picked so that a 4-core host can complete
/// the iteration in a few seconds without thrashing the init
/// pipeline's serialized phases too hard.
const N: usize = 10;

/// `density-parallel-10` — concurrent spawn density.
pub struct DensityParallel10 {
    home: Option<TempDir>,
    prewarmed: bool,
}

impl DensityParallel10 {
    pub fn new() -> Self {
        Self {
            home: None,
            prewarmed: false,
        }
    }
}

#[async_trait]
impl Scenario for DensityParallel10 {
    fn name(&self) -> &str {
        "density-parallel-10"
    }

    async fn run_once(&mut self, ctx: &RunContext) -> Result<BTreeMap<String, f64>> {
        if self.home.is_none() {
            self.home = Some(TempDir::new().context("mkdir density-parallel home")?);
        }
        let home_path = self
            .home
            .as_ref()
            .expect("just initialized")
            .path()
            .to_path_buf();
        let rt = build_runtime(ctx.global, home_path)?;

        // First-call pre-warm: spawn one throwaway box so the
        // image, base disk, and guest rootfs are on disk before the
        // measured burst. Without this, the first iteration's
        // per-box latencies would be dominated by image-pull
        // contention, not init-pipeline contention — defeating the
        // headline.
        if !self.prewarmed {
            let warm = rt
                .create(alpine_options(), None)
                .await
                .context("density pre-warm create")?;
            warm.start().await.context("density pre-warm start")?;
            warm.stop().await.context("density pre-warm stop")?;
            self.prewarmed = true;
        }

        let latencies = spawn_burst(&rt, N).await?;

        let mut metrics = BTreeMap::new();
        let max = latencies.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let mean = latencies.iter().copied().sum::<f64>() / latencies.len() as f64;
        metrics.insert("parallel_max_latency_ms".into(), max);
        metrics.insert("parallel_mean_latency_ms".into(), mean);
        metrics.insert("parallel_boxes_count".into(), N as f64);

        Ok(metrics)
    }
}

/// Spawn `n` boxes concurrently against the same runtime, wait for
/// every one to reach Running (or error), tear them all down, and
/// return each box's individual create+start duration in
/// milliseconds.
///
/// Implementation note: we collect `JoinHandle`s before awaiting
/// any so the boxes spawn truly in parallel (the alternative of
/// `for i in 0..n { let dur = tokio::spawn(...).await?; }` would
/// serialize them on the main task's `await`). Each task owns its
/// `BoxGuard`, so a panic in any single task doesn't leak the
/// rest of the fleet — Drop of the guard force-removes its box.
async fn spawn_burst(rt: &BoxliteRuntime, n: usize) -> Result<Vec<f64>> {
    let mut handles = Vec::with_capacity(n);
    for _ in 0..n {
        let rt = rt.clone();
        let handle = tokio::spawn(async move {
            let start = Instant::now();
            let live = rt
                .create(alpine_options(), None)
                .await
                .context("rt.create in burst")?;
            let mut guard = BoxGuard::new(&rt, live.id().to_string());
            live.start().await.context("box.start in burst")?;
            let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
            // Hand the box back to the harness for cleanup *after* we
            // collected the latency, so the stop()/auto_remove cost
            // doesn't pollute the measured Running-transition time.
            live.stop().await.ok();
            guard.disarm();
            Ok::<f64, anyhow::Error>(elapsed_ms)
        });
        handles.push(handle);
    }

    let mut latencies = Vec::with_capacity(n);
    let mut errors = Vec::new();
    for handle in handles {
        match handle.await {
            Ok(Ok(ms)) => latencies.push(ms),
            Ok(Err(e)) => errors.push(e),
            Err(join_err) => errors.push(anyhow::anyhow!("task join: {join_err}")),
        }
    }
    if !errors.is_empty() {
        anyhow::bail!(
            "density burst had {} failure(s) (out of {}): {}",
            errors.len(),
            n,
            errors
                .iter()
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
                .join("; "),
        );
    }
    Ok(latencies)
}

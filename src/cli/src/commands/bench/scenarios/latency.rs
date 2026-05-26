//! Latency scenarios: how long does one operation take.
//!
//! Two scenarios, both driving `alpine:latest`:
//!   * `latency-cold-start` — every iteration uses a fresh `--home`
//!     so the image cache, base disk, and guest rootfs are all
//!     rebuilt. Numbers reflect first-box-on-a-fresh-machine cost.
//!   * `latency-warm-start` — one shared `--home` across iterations
//!     (warmed by the first iteration); each measured iteration is
//!     "second+ box on a host that already pulled this image and
//!     bootstrapped the guest rootfs cache." Numbers reflect the
//!     steady-state create-box cost.
//!
//! Both populate the same per-stage metrics from `BoxMetrics`
//! (`total_create_ms`, `stage_filesystem_setup_ms`,
//! `stage_image_prepare_ms`, `stage_guest_rootfs_ms`,
//! `stage_box_spawn_ms`, `stage_container_init_ms`,
//! `guest_boot_ms`), so the cold-vs-warm delta on
//! `stage_image_prepare_ms` directly attributes the
//! image-pull-and-extract cost.
//!
//! Box lifecycle inside one iteration: `create` → `start` →
//! `metrics` snapshot → `stop`. `auto_remove=true` (boxlite's
//! default) wipes the box record + on-disk state at `stop`, so the
//! per-iteration leak surface is zero on the happy path. A
//! [`BoxGuard`] RAII wrapper SIGKILLs the libkrun VM at scope end
//! to mop up if any step before `stop` panicked.

use super::super::runner::{RunContext, Scenario};
use super::common::{BoxGuard, alpine_options, build_runtime};
use anyhow::{Context, Result};
use async_trait::async_trait;
use boxlite::{BoxliteRuntime, LiteBox};
use std::collections::BTreeMap;
use tempfile::TempDir;

/// Pull every `BoxMetrics` field we care about for latency into a
/// flat name→value map for the report. Names use the `_ms` suffix so
/// the report's unit hint resolves to milliseconds.
async fn populate_stage_metrics(live: &LiteBox, out: &mut BTreeMap<String, f64>) -> Result<()> {
    let m = live.metrics().await.context("snapshot BoxMetrics")?;
    if let Some(v) = m.total_create_duration_ms() {
        out.insert("total_create_ms".into(), v as f64);
    }
    if let Some(v) = m.guest_boot_duration_ms() {
        out.insert("guest_boot_ms".into(), v as f64);
    }
    if let Some(v) = m.stage_filesystem_setup_ms() {
        out.insert("stage_filesystem_setup_ms".into(), v as f64);
    }
    if let Some(v) = m.stage_image_prepare_ms() {
        out.insert("stage_image_prepare_ms".into(), v as f64);
    }
    if let Some(v) = m.stage_guest_rootfs_ms() {
        out.insert("stage_guest_rootfs_ms".into(), v as f64);
    }
    if let Some(v) = m.stage_box_config_ms() {
        out.insert("stage_box_config_ms".into(), v as f64);
    }
    if let Some(v) = m.stage_box_spawn_ms() {
        out.insert("stage_box_spawn_ms".into(), v as f64);
    }
    if let Some(v) = m.stage_container_init_ms() {
        out.insert("stage_container_init_ms".into(), v as f64);
    }
    Ok(())
}

/// Drive one create→start→metrics→stop cycle and return the populated
/// metric map. Used by both scenarios; the only thing that differs
/// between them is whether the home is fresh (cold) or shared (warm).
async fn drive_one(rt: &BoxliteRuntime) -> Result<BTreeMap<String, f64>> {
    let opts = alpine_options();
    let live = rt.create(opts, None).await.context("rt.create(alpine)")?;
    let mut guard = BoxGuard::new(rt, live.id().to_string());

    live.start().await.context("box.start()")?;

    let mut metrics = BTreeMap::new();
    populate_stage_metrics(&live, &mut metrics).await?;

    // auto_remove=true means stop() also wipes the record from disk
    // and DB. On error we leak just like any other CLI invocation
    // would — the BoxGuard above mops up.
    live.stop().await.context("box.stop()")?;
    guard.disarm();

    Ok(metrics)
}

// ─── cold-start ────────────────────────────────────────────────────

/// Every iteration uses a fresh `TempDir`-backed home. The image
/// pull, base disk build, and guest rootfs bootstrap all run from
/// scratch, so the numbers reflect "boxlite on a fresh machine."
pub struct ColdStart {
    /// The previous iteration's home, held only so its `Drop` runs
    /// inside `after_iteration` rather than racing with the next
    /// iteration's bench cycle.
    previous_home: Option<TempDir>,
}

impl ColdStart {
    pub fn new() -> Self {
        Self {
            previous_home: None,
        }
    }
}

#[async_trait]
impl Scenario for ColdStart {
    fn name(&self) -> &str {
        "latency-cold-start"
    }

    async fn run_once(&mut self, ctx: &RunContext) -> Result<BTreeMap<String, f64>> {
        let tmp = TempDir::new().context("mkdir cold-start home")?;
        let rt = build_runtime(ctx.global, tmp.path().to_path_buf())?;
        let metrics = drive_one(&rt).await?;
        // Hand ownership of `tmp` to `previous_home`; it'll be
        // dropped (and the dir removed) when the next iteration
        // arrives. This decouples the temp-dir teardown cost — which
        // can be hundreds of ms on a large base disk — from the
        // measured `wall_ms` of THIS iteration.
        self.previous_home = Some(tmp);
        Ok(metrics)
    }
}

// ─── warm-start ────────────────────────────────────────────────────

/// One home across all iterations. The first iteration pays the
/// image-pull + base-disk-build + guest-rootfs-bootstrap cost
/// (typically hidden behind `--warmup`); subsequent iterations are
/// steady-state second-box-onwards numbers.
pub struct WarmStart {
    home: Option<TempDir>,
}

impl WarmStart {
    pub fn new() -> Self {
        Self { home: None }
    }
}

#[async_trait]
impl Scenario for WarmStart {
    fn name(&self) -> &str {
        "latency-warm-start"
    }

    async fn run_once(&mut self, ctx: &RunContext) -> Result<BTreeMap<String, f64>> {
        // Lazily allocate the shared home on first call so the
        // `Default::default()` constructor of the scenario stays
        // infallible (the `tempfile::TempDir::new()` IO failure
        // surfaces here instead, where we have an `Err` channel).
        if self.home.is_none() {
            self.home = Some(TempDir::new().context("mkdir warm-start home")?);
        }
        let home_path = self
            .home
            .as_ref()
            .expect("just initialized")
            .path()
            .to_path_buf();
        let rt = build_runtime(ctx.global, home_path)?;
        drive_one(&rt).await
    }
}

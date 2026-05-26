//! Cold-start with a non-trivial image. `latency-cold-start` uses
//! `alpine:latest` (~3 MB compressed); this scenario uses
//! `python:3.12-alpine` (~50 MB compressed, 7-8 layers) to exercise
//! the layer-tarball-extraction and qcow2-base-build paths at a
//! scale where stage_image_prepare_ms is no longer dominated by
//! HTTP round-trips. The image-size-vs-time curve falls out of
//! comparing this against `throughput-image-pull` headline MB/s.
//!
//! Per iteration:
//!   * Fresh `--home`.
//!   * Pull `python:3.12-alpine`, create + start, snapshot stage
//!     metrics, stop.
//!
//! Reports the standard per-stage timings so a direct diff against
//! `latency-cold-start` reveals where the size-dependent stages
//! (image_prepare, guest_rootfs) eat their proportional cost.

use super::super::runner::{RunContext, Scenario};
use super::common::{BoxGuard, build_runtime};
use anyhow::{Context, Result};
use async_trait::async_trait;
use boxlite::BoxOptions;
use boxlite::runtime::options::RootfsSpec;
use std::collections::BTreeMap;
use tempfile::TempDir;

/// Picked because it's a multi-layer image (python interpreter +
/// alpine base + pip + lib stubs) that exercises the per-layer
/// unpack path several times in one cold-start, and is small enough
/// to land in under 30 s on a typical CI runner without saturating
/// disk.
const IMAGE: &str = "python:3.12-alpine";

pub struct LatencyColdStartBigImage {
    previous_home: Option<TempDir>,
}

impl LatencyColdStartBigImage {
    pub fn new() -> Self {
        Self {
            previous_home: None,
        }
    }
}

#[async_trait]
impl Scenario for LatencyColdStartBigImage {
    fn name(&self) -> &str {
        "latency-cold-start-big-image"
    }

    async fn run_once(&mut self, ctx: &RunContext) -> Result<BTreeMap<String, f64>> {
        let tmp = TempDir::new().context("mkdir big-image home")?;
        let rt = build_runtime(ctx.global, tmp.path().to_path_buf())?;

        let opts = BoxOptions {
            rootfs: RootfsSpec::Image(IMAGE.into()),
            auto_remove: true,
            ..Default::default()
        };
        let live = rt
            .create(opts, None)
            .await
            .with_context(|| format!("rt.create({IMAGE})"))?;
        let mut guard = BoxGuard::new(&rt, live.id().to_string());
        live.start().await.context("box.start()")?;

        let m = live.metrics().await.context("snapshot BoxMetrics")?;
        let mut metrics = BTreeMap::new();
        if let Some(v) = m.total_create_duration_ms() {
            metrics.insert("total_create_ms".into(), v as f64);
        }
        if let Some(v) = m.guest_boot_duration_ms() {
            metrics.insert("guest_boot_ms".into(), v as f64);
        }
        if let Some(v) = m.stage_filesystem_setup_ms() {
            metrics.insert("stage_filesystem_setup_ms".into(), v as f64);
        }
        if let Some(v) = m.stage_image_prepare_ms() {
            metrics.insert("stage_image_prepare_ms".into(), v as f64);
        }
        if let Some(v) = m.stage_guest_rootfs_ms() {
            metrics.insert("stage_guest_rootfs_ms".into(), v as f64);
        }
        if let Some(v) = m.stage_box_config_ms() {
            metrics.insert("stage_box_config_ms".into(), v as f64);
        }
        if let Some(v) = m.stage_box_spawn_ms() {
            metrics.insert("stage_box_spawn_ms".into(), v as f64);
        }
        if let Some(v) = m.stage_container_init_ms() {
            metrics.insert("stage_container_init_ms".into(), v as f64);
        }

        live.stop().await.context("box.stop()")?;
        guard.disarm();
        self.previous_home = Some(tmp);
        Ok(metrics)
    }
}

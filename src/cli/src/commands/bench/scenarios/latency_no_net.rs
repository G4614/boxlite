//! Cold-start with `NetworkSpec::Disabled` — gvproxy is not started
//! and the guest gets no `eth0`. The delta against `latency-cold-
//! start` is the gvproxy boot cost, which is interesting on its own:
//! compute-only workloads that don't need network can shave whatever
//! that delta is off every box bring-up.
//!
//! Per iteration:
//!   * Fresh `--home` (so the comparison against `latency-cold-start`
//!     stays apples-to-apples).
//!   * `BoxOptions::network = NetworkSpec::Disabled`.
//!   * Otherwise identical to `latency-cold-start`: create → start →
//!     metrics snapshot → stop.
//!
//! Reports the same per-stage BoxMetrics as the other latency
//! scenarios so direct field-vs-field diffs are meaningful.

use super::super::runner::{RunContext, Scenario};
use super::common::{BoxGuard, build_runtime};
use anyhow::{Context, Result};
use async_trait::async_trait;
use boxlite::BoxOptions;
use boxlite::runtime::options::{NetworkSpec, RootfsSpec};
use std::collections::BTreeMap;
use tempfile::TempDir;

pub struct LatencyColdStartNoNet {
    previous_home: Option<TempDir>,
}

impl LatencyColdStartNoNet {
    pub fn new() -> Self {
        Self {
            previous_home: None,
        }
    }
}

#[async_trait]
impl Scenario for LatencyColdStartNoNet {
    fn name(&self) -> &str {
        "latency-cold-start-no-net"
    }

    async fn run_once(&mut self, ctx: &RunContext) -> Result<BTreeMap<String, f64>> {
        let tmp = TempDir::new().context("mkdir no-net home")?;
        let rt = build_runtime(ctx.global, tmp.path().to_path_buf())?;

        let opts = BoxOptions {
            rootfs: RootfsSpec::Image("alpine:latest".into()),
            auto_remove: true,
            network: NetworkSpec::Disabled,
            ..Default::default()
        };
        let live = rt.create(opts, None).await.context("rt.create(alpine)")?;
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

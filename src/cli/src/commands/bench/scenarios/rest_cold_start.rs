//! Cold-start over the REST API. Most SDK clients (Python, Node, Go,
//! and the `boxlite` CLI when `--url` is set) talk to a `boxlite
//! serve` over HTTP. The latency we report under `latency-cold-start`
//! is the in-process path; this scenario measures the same operation
//! end-to-end through the REST surface so the difference quantifies
//! the axum + tower + serde + HTTP-RTT tax.
//!
//! Per iteration:
//!   1. Spawn `boxlite serve` child against a fresh home.
//!   2. Build a `BoxliteRuntime::rest(url)` aimed at the child.
//!   3. Cold-start a box via the REST runtime: `rt.create + start`.
//!   4. Snapshot BoxMetrics (over REST too — the call rides the
//!      same HTTP path).
//!   5. `rt.shutdown` cleans the box; child dies on Drop.
//!
//! Reports `total_create_ms` plus the per-stage breakdown, exactly
//! like `latency-cold-start`. Delta = REST overhead.

use super::super::runner::{RunContext, Scenario};
use super::common::ServeChild;
use anyhow::{Context, Result};
use async_trait::async_trait;
use boxlite::runtime::options::RootfsSpec;
use boxlite::{BoxOptions, BoxliteRestOptions, BoxliteRuntime, LiteBox};
use std::collections::BTreeMap;

pub struct RestColdStart;

impl RestColdStart {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Scenario for RestColdStart {
    fn name(&self) -> &str {
        "latency-rest-cold-start"
    }

    async fn run_once(&mut self, ctx: &RunContext) -> Result<BTreeMap<String, f64>> {
        // ServeChild::Drop sends SIGKILL when this binding goes out
        // of scope; explicit so it lasts through the entire iter.
        let server = ServeChild::spawn("rest-cold-start", &ctx.global.registry).await?;

        let rest_opts = BoxliteRestOptions::new(&server.url);
        let rt = BoxliteRuntime::rest(rest_opts).context("BoxliteRuntime::rest")?;

        let opts = BoxOptions {
            rootfs: RootfsSpec::Image("alpine:latest".into()),
            auto_remove: true,
            ..Default::default()
        };
        let live: LiteBox = rt
            .create(opts, None)
            .await
            .context("rt.create(alpine) over REST")?;
        live.start().await.context("box.start() over REST")?;

        let m = live
            .metrics()
            .await
            .context("snapshot BoxMetrics over REST")?;
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

        // Explicit stop so the in-server box record drains before
        // the child is killed (kill_on_drop is heavy-handed).
        let _ = live.stop().await;
        drop(server);
        Ok(metrics)
    }
}

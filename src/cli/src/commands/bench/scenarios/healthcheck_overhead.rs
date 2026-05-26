//! Healthcheck-on overhead. A box with `HealthCheckOptions` set
//! triggers a background ping every `interval` to the guest agent;
//! the shim wakes, the guest agent responds, the shim records the
//! result, repeat. At the default 30 s interval the overhead is
//! basically zero on any RSS/CPU sample; this scenario tightens
//! `interval` to 500 ms so the overhead is observable over a 10 s
//! sample window — and that overhead × (real-world interval / 500 ms)
//! is the real number you'd extrapolate for production tuning.
//!
//! Per iteration:
//!   * Box created with `interval=500ms`, `timeout=200ms`,
//!     `retries=3`, `start_period=1s` (low so the count-toward-
//!     unhealthy clock starts fast and we observe actual pings, not
//!     warmup-period skips).
//!   * Wait `WARMUP_SECS` for start_period to drain and ping cadence
//!     to settle.
//!   * Sample `BoxMetrics` every 1 s for `SAMPLE_SECS`. Report
//!     mean+max CPU% and peak RSS while healthcheck is active.
//!
//! To get the overhead number, diff against `resource-idle` on the
//! same host: that scenario uses an identical alpine box with no
//! healthcheck.

use super::super::runner::{RunContext, Scenario};
use super::common::{BoxGuard, build_runtime};
use anyhow::{Context, Result};
use async_trait::async_trait;
use boxlite::BoxOptions;
use boxlite::runtime::advanced_options::{AdvancedBoxOptions, HealthCheckOptions};
use boxlite::runtime::options::RootfsSpec;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};
use tempfile::TempDir;

const WARMUP_SECS: u64 = 3;
const SAMPLE_SECS: u64 = 10;
const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);
const HEALTHCHECK_INTERVAL_MS: u64 = 500;

pub struct HealthcheckOverhead {
    home: Option<TempDir>,
}

impl HealthcheckOverhead {
    pub fn new() -> Self {
        Self { home: None }
    }
}

#[async_trait]
impl Scenario for HealthcheckOverhead {
    fn name(&self) -> &str {
        "resource-healthcheck-overhead"
    }

    async fn run_once(&mut self, ctx: &RunContext) -> Result<BTreeMap<String, f64>> {
        if self.home.is_none() {
            self.home = Some(TempDir::new().context("mkdir healthcheck home")?);
        }
        let home_path = self
            .home
            .as_ref()
            .expect("just initialized")
            .path()
            .to_path_buf();
        let rt = build_runtime(ctx.global, home_path)?;

        let opts = BoxOptions {
            rootfs: RootfsSpec::Image("alpine:latest".into()),
            auto_remove: true,
            advanced: AdvancedBoxOptions {
                health_check: Some(HealthCheckOptions {
                    interval: Duration::from_millis(HEALTHCHECK_INTERVAL_MS),
                    timeout: Duration::from_millis(200),
                    retries: 3,
                    start_period: Duration::from_secs(1),
                }),
                ..AdvancedBoxOptions::default()
            },
            ..Default::default()
        };
        let live = rt
            .create(opts, None)
            .await
            .context("rt.create(alpine + healthcheck)")?;
        let mut guard = BoxGuard::new(&rt, live.id().to_string());
        live.start().await.context("box.start()")?;

        // Drain start_period so healthcheck cadence is steady-state
        // when sampling begins.
        tokio::time::sleep(Duration::from_secs(WARMUP_SECS)).await;

        let mut rss_samples: Vec<u64> = Vec::new();
        let mut cpu_samples: Vec<f32> = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(SAMPLE_SECS);
        while Instant::now() < deadline {
            let snap = live
                .metrics()
                .await
                .context("snap BoxMetrics under healthcheck")?;
            if let Some(r) = snap.memory_bytes() {
                rss_samples.push(r);
            }
            if let Some(c) = snap.cpu_percent() {
                cpu_samples.push(c);
            }
            tokio::time::sleep(SAMPLE_INTERVAL).await;
        }

        let mut metrics = BTreeMap::new();
        metrics.insert(
            "healthcheck_interval_ms".into(),
            HEALTHCHECK_INTERVAL_MS as f64,
        );
        metrics.insert("healthcheck_samples_count".into(), rss_samples.len() as f64);
        if !rss_samples.is_empty() {
            let max = rss_samples.iter().copied().max().unwrap_or(0);
            let mean = rss_samples.iter().copied().sum::<u64>() / rss_samples.len() as u64;
            metrics.insert("healthcheck_rss_max_bytes".into(), max as f64);
            metrics.insert("healthcheck_rss_mean_bytes".into(), mean as f64);
        }
        if !cpu_samples.is_empty() {
            let max = cpu_samples
                .iter()
                .copied()
                .fold(f32::NEG_INFINITY, f32::max);
            let mean: f32 = cpu_samples.iter().copied().sum::<f32>() / cpu_samples.len() as f32;
            metrics.insert("healthcheck_cpu_pct_max".into(), max as f64);
            metrics.insert("healthcheck_cpu_pct_mean".into(), mean as f64);
        }

        live.stop().await.context("box.stop()")?;
        guard.disarm();
        Ok(metrics)
    }
}

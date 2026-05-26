//! `BoxliteRuntime::metrics()` polling cost at population N.
//!
//! Distinct from the per-box `BoxMetrics` sampling done by
//! `resource-idle` etc. Here we measure the cost of the runtime-wide
//! aggregate snapshot — `num_running_boxes`, `boxes_created_total`,
//! `total_commands_executed`, etc. Callers that scrape this every
//! second for Prometheus need to know what one `rt.metrics()` call
//! actually costs as the box population grows.
//!
//! Per iteration: stage N=10 idle boxes (once per scenario instance),
//! then poll `rt.metrics()` 500 times in a tight loop. Reports
//! mean / p50 / p99 / max in microseconds.

use super::super::runner::{RunContext, Scenario};
use super::common::{alpine_options, build_runtime};
use anyhow::{Context, Result};
use async_trait::async_trait;
use boxlite::LiteBox;
use std::collections::BTreeMap;
use std::time::Instant;
use tempfile::TempDir;

/// Nearest-rank percentile, 1-indexed (same definition as
/// `stats::aggregate`). Inline because that helper is private to
/// stats.rs and we only need it locally.
fn nearest_rank(sorted: &[f64], p: u32) -> f64 {
    let n = sorted.len();
    let rank = ((p as f64 / 100.0) * n as f64).ceil() as usize;
    let idx = rank.clamp(1, n) - 1;
    sorted[idx]
}

const N_BOXES: usize = 10;
const N_POLLS: usize = 500;

pub struct RuntimeMetricsPoll {
    home: Option<TempDir>,
    /// Hold live handles so the boxes stay running across iterations
    /// (otherwise auto_remove=false alone doesn't keep them in the
    /// "running" set that RuntimeMetrics counts).
    boxes: Vec<LiteBox>,
}

impl RuntimeMetricsPoll {
    pub fn new() -> Self {
        Self {
            home: None,
            boxes: Vec::new(),
        }
    }
}

#[async_trait]
impl Scenario for RuntimeMetricsPoll {
    fn name(&self) -> &str {
        "resource-runtime-metrics-poll"
    }

    async fn run_once(&mut self, ctx: &RunContext) -> Result<BTreeMap<String, f64>> {
        if self.home.is_none() {
            self.home = Some(TempDir::new().context("mkdir rt-metrics home")?);
        }
        let home_path = self
            .home
            .as_ref()
            .expect("just initialized")
            .path()
            .to_path_buf();
        let rt = build_runtime(ctx.global, home_path)?;

        if self.boxes.is_empty() {
            for _ in 0..N_BOXES {
                let mut opts = alpine_options();
                opts.auto_remove = false;
                let b = rt.create(opts, None).await.context("rt.create(staged)")?;
                b.start().await.context("staged box.start")?;
                self.boxes.push(b);
            }
        }

        let mut samples_us: Vec<f64> = Vec::with_capacity(N_POLLS);
        for _ in 0..N_POLLS {
            let t = Instant::now();
            let _m = rt.metrics().await.context("rt.metrics")?;
            samples_us.push(t.elapsed().as_secs_f64() * 1_000_000.0);
        }

        let mean = samples_us.iter().copied().sum::<f64>() / samples_us.len() as f64;
        let max = samples_us.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let mut sorted = samples_us.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

        let mut metrics = BTreeMap::new();
        metrics.insert("rt_metrics_population".into(), N_BOXES as f64);
        metrics.insert("rt_metrics_polls".into(), N_POLLS as f64);
        metrics.insert("rt_metrics_mean_us".into(), mean);
        metrics.insert("rt_metrics_p50_us".into(), nearest_rank(&sorted, 50));
        metrics.insert("rt_metrics_p99_us".into(), nearest_rank(&sorted, 99));
        metrics.insert("rt_metrics_max_us".into(), max);
        Ok(metrics)
    }
}

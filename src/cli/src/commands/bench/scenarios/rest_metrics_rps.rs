//! `GET /v1/metrics` RPS. Same shape as `throughput-serve-rps`
//! (`/v1/config`) but against the metrics endpoint that wraps
//! `BoxliteRuntime::metrics()`. Prometheus-style scrapes hit this
//! every interval, so the floor RPS here caps how dense you can
//! make scrape intervals without serve becoming the bottleneck.
//!
//! In-process `rt.metrics()` is 0.2 µs (see `resource-runtime-
//! metrics-poll`); this scenario reports the same op wrapped in an
//! HTTP serializer + axum route + tower middleware. The gap is the
//! REST overhead.

use super::super::runner::{RunContext, Scenario};
use super::common::ServeChild;
use anyhow::{Context, Result};
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const WINDOW_SECS: u64 = 5;
const CONCURRENCY: usize = 16;
const REQ_TIMEOUT: Duration = Duration::from_secs(5);

pub struct RestMetricsRps;

impl RestMetricsRps {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Scenario for RestMetricsRps {
    fn name(&self) -> &str {
        "throughput-rest-metrics-rps"
    }

    async fn run_once(&mut self, ctx: &RunContext) -> Result<BTreeMap<String, f64>> {
        let server = ServeChild::spawn("rest-metrics-rps", &ctx.global.registry).await?;
        let url = format!("{}/v1/metrics", server.url);

        let client = reqwest::Client::builder()
            .timeout(REQ_TIMEOUT)
            .build()
            .context("build reqwest client")?;

        let success = Arc::new(AtomicU64::new(0));
        let errors = Arc::new(AtomicU64::new(0));
        let stop_at = Instant::now() + Duration::from_secs(WINDOW_SECS);
        let started = Instant::now();
        let mut handles = Vec::with_capacity(CONCURRENCY);
        for _ in 0..CONCURRENCY {
            let client = client.clone();
            let url = url.clone();
            let success = Arc::clone(&success);
            let errors = Arc::clone(&errors);
            handles.push(tokio::spawn(async move {
                while Instant::now() < stop_at {
                    match client.get(&url).send().await {
                        Ok(r) if r.status().is_success() => {
                            success.fetch_add(1, Ordering::Relaxed);
                        }
                        _ => {
                            errors.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            }));
        }
        for h in handles {
            let _ = h.await;
        }
        let elapsed = started.elapsed().as_secs_f64();

        let succ = success.load(Ordering::Relaxed);
        let err = errors.load(Ordering::Relaxed);

        let mut metrics = BTreeMap::new();
        if elapsed > 0.0 {
            metrics.insert("rest_metrics_rps".into(), succ as f64 / elapsed);
        }
        metrics.insert("rest_metrics_success_count".into(), succ as f64);
        metrics.insert("rest_metrics_error_count".into(), err as f64);
        metrics.insert("rest_metrics_concurrency".into(), CONCURRENCY as f64);

        drop(server);
        Ok(metrics)
    }
}

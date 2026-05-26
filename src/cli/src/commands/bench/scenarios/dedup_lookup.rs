//! `BoxliteRuntime::get_or_create` dedup-path latency.
//!
//! On the create code path, `get_or_create` is documented as
//! "Get an existing box by name, or create a new one if it
//! doesn't exist". The hot path for repeated invocations with the
//! same name is the dedup-hit path: SQLite name → box-id lookup
//! plus `LiteBox` materialization. This scenario measures it.
//!
//! Per iteration:
//!   1. One-time setup: pick a stable name, call get_or_create
//!      once to materialize the box (first-call = create path).
//!   2. Hot loop: call get_or_create(name) N=100 times; every
//!      call lands on the dedup-hit path.
//!
//! Reports:
//!   * `dedup_count` — N (always 100).
//!   * `dedup_first_create_ms` — first call (create path), reported
//!     for context. Should look like a warm-start latency.
//!   * `dedup_hit_mean_us`, `dedup_hit_max_us` — hot-loop stats.
//!     Floor number for "how cheap is name→box resolution".

use super::super::runner::{RunContext, Scenario};
use super::common::{alpine_options, build_runtime};
use anyhow::{Context, Result};
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::time::Instant;
use tempfile::TempDir;

const N_HITS: usize = 100;

pub struct DedupLookup {
    home: Option<TempDir>,
    materialized_name: Option<String>,
    first_create_ms: Option<f64>,
}

impl DedupLookup {
    pub fn new() -> Self {
        Self {
            home: None,
            materialized_name: None,
            first_create_ms: None,
        }
    }
}

#[async_trait]
impl Scenario for DedupLookup {
    fn name(&self) -> &str {
        "latency-get-or-create-dedup"
    }

    async fn run_once(&mut self, ctx: &RunContext) -> Result<BTreeMap<String, f64>> {
        if self.home.is_none() {
            self.home = Some(TempDir::new().context("mkdir dedup home")?);
        }
        let home_path = self
            .home
            .as_ref()
            .expect("just initialized")
            .path()
            .to_path_buf();
        let rt = build_runtime(ctx.global, home_path)?;

        if self.materialized_name.is_none() {
            let name = "bench-dedup-target".to_string();
            let t0 = Instant::now();
            let (_b, _created) = rt
                .get_or_create(alpine_options(), Some(name.clone()))
                .await
                .context("rt.get_or_create(first)")?;
            self.first_create_ms = Some(t0.elapsed().as_secs_f64() * 1000.0);
            self.materialized_name = Some(name);
        }
        let name = self.materialized_name.as_ref().expect("set").clone();

        let mut samples_us: Vec<f64> = Vec::with_capacity(N_HITS);
        for _ in 0..N_HITS {
            let t = Instant::now();
            let (_b, _created) = rt
                .get_or_create(alpine_options(), Some(name.clone()))
                .await
                .context("rt.get_or_create(hit)")?;
            samples_us.push(t.elapsed().as_secs_f64() * 1_000_000.0);
        }

        let mean = samples_us.iter().copied().sum::<f64>() / samples_us.len() as f64;
        let max = samples_us.iter().copied().fold(f64::NEG_INFINITY, f64::max);

        let mut metrics = BTreeMap::new();
        metrics.insert("dedup_count".into(), N_HITS as f64);
        if let Some(v) = self.first_create_ms {
            metrics.insert("dedup_first_create_ms".into(), v);
        }
        metrics.insert("dedup_hit_mean_us".into(), mean);
        metrics.insert("dedup_hit_max_us".into(), max);
        Ok(metrics)
    }
}

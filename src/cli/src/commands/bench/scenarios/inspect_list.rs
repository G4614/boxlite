//! `BoxliteRuntime::list_info` + `get_info` latency at N boxes.
//!
//! Both ops go through boxlite's SQLite-backed box store. As the
//! box population grows, list latency grows roughly linearly
//! with row count, and `get_info` should stay roughly constant
//! (indexed lookup by id). This scenario stages N=20 sleeping
//! alpine boxes and times both ops.
//!
//! Reports:
//!   * `list_info_count` — N (population size).
//!   * `list_info_ms` — `rt.list_info()` wall.
//!   * `get_info_mean_ms` — mean of 20 `rt.get_info(<id>)` calls
//!     across all N boxes.
//!   * `get_info_max_ms` — slowest `get_info` (tail).

use super::super::runner::{RunContext, Scenario, TeardownContext};
use super::common::{alpine_options, build_runtime};
use anyhow::{Context, Result};
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::time::Instant;
use tempfile::TempDir;

const N: usize = 20;

pub struct InspectList {
    home: Option<TempDir>,
    box_ids: Vec<String>,
}

impl InspectList {
    pub fn new() -> Self {
        Self {
            home: None,
            box_ids: Vec::new(),
        }
    }
}

#[async_trait]
impl Scenario for InspectList {
    fn name(&self) -> &str {
        "latency-inspect-list"
    }

    async fn run_once(&mut self, ctx: &RunContext) -> Result<BTreeMap<String, f64>> {
        if self.home.is_none() {
            self.home = Some(TempDir::new().context("mkdir inspect-list home")?);
        }
        let home_path = self
            .home
            .as_ref()
            .expect("just initialized")
            .path()
            .to_path_buf();
        let rt = build_runtime(ctx.global, home_path)?;

        // One-time stage: create N boxes (sequentially — cold
        // start cost is amortized across iterations via the
        // shared home; subsequent iterations re-find the same
        // boxes still in the DB).
        if self.box_ids.is_empty() {
            for _ in 0..N {
                let mut opts = alpine_options();
                opts.auto_remove = false;
                let b = rt.create(opts, None).await.context("rt.create #")?;
                let id = b.id().to_string();
                // Don't start them — just create + DB row is
                // enough to exercise list/get.
                self.box_ids.push(id);
            }
        }

        // Time list_info.
        let t0 = Instant::now();
        let infos = rt.list_info().await.context("list_info")?;
        let list_ms = t0.elapsed().as_secs_f64() * 1000.0;

        // get_info for each staged id.
        let mut times = Vec::with_capacity(self.box_ids.len());
        for id in &self.box_ids {
            let t = Instant::now();
            let _info = rt
                .get_info(id)
                .await
                .with_context(|| format!("get_info({id})"))?;
            times.push(t.elapsed().as_secs_f64() * 1000.0);
        }

        let mean = times.iter().copied().sum::<f64>() / times.len() as f64;
        let max = times.iter().copied().fold(f64::NEG_INFINITY, f64::max);

        let mut metrics = BTreeMap::new();
        metrics.insert("list_info_count".into(), infos.len() as f64);
        metrics.insert("list_info_ms".into(), list_ms);
        metrics.insert("get_info_mean_ms".into(), mean);
        metrics.insert("get_info_max_ms".into(), max);
        metrics.insert("get_info_samples_count".into(), times.len() as f64);
        Ok(metrics)
    }

    async fn teardown(&mut self, ctx: &TeardownContext<'_>) -> Result<()> {
        let Some(home) = self.home.as_ref() else {
            return Ok(());
        };
        if self.box_ids.is_empty() {
            return Ok(());
        }
        let rt = build_runtime(ctx.global, home.path().to_path_buf())?;
        // 20 created-but-never-started boxes; remove each.
        for id in self.box_ids.drain(..) {
            let _ = rt.remove(&id, true).await;
        }
        Ok(())
    }
}

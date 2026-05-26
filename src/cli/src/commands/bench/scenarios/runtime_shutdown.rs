//! `BoxliteRuntime::shutdown` latency at N running boxes.
//!
//! Tests the graceful-shutdown path that container orchestrators
//! rely on for clean termination — SIGTERM → boxlite gets a window
//! to stop each box, then SIGKILLs whatever didn't make it.
//!
//! Per iteration:
//!   1. Build a fresh runtime (each iter must build its own —
//!      `shutdown` permanently disables the runtime, so reuse isn't
//!      possible).
//!   2. Create + start N=3 idle alpine boxes (kept small because
//!      every iteration pays the cold-start tax thrice).
//!   3. Time `rt.shutdown(timeout=None)` (default 10 s per box).
//!
//! Reports:
//!   * `shutdown_n_boxes` — N (always 3).
//!   * `shutdown_ms` — `rt.shutdown` wall. Headline: how long
//!     does graceful-stop take per box on average? Regressions
//!     here directly hurt container-orchestration shutdown SLAs.

use super::super::runner::{RunContext, Scenario};
use super::common::{alpine_options, build_runtime};
use anyhow::{Context, Result};
use async_trait::async_trait;
use boxlite::LiteBox;
use std::collections::BTreeMap;
use std::time::Instant;
use tempfile::TempDir;

const N_BOXES: usize = 3;

pub struct RuntimeShutdown {
    /// Reuse the same home across iterations so the image cache and
    /// base disk are warm — otherwise every iteration would also
    /// pay 3× cold-pull and the headline would be drowned in
    /// image-pull cost.
    home: Option<TempDir>,
}

impl RuntimeShutdown {
    pub fn new() -> Self {
        Self { home: None }
    }
}

#[async_trait]
impl Scenario for RuntimeShutdown {
    fn name(&self) -> &str {
        "latency-runtime-shutdown"
    }

    async fn run_once(&mut self, ctx: &RunContext) -> Result<BTreeMap<String, f64>> {
        if self.home.is_none() {
            self.home = Some(TempDir::new().context("mkdir shutdown home")?);
        }
        let home_path = self
            .home
            .as_ref()
            .expect("just initialized")
            .path()
            .to_path_buf();
        let rt = build_runtime(ctx.global, home_path)?;

        // Stage N running boxes. Held in a Vec until after shutdown
        // so the per-box auto-stop-on-Drop (triggered by
        // `auto_remove=true`) doesn't fire before `shutdown` runs
        // and leave the runtime with zero active boxes to do work
        // on — the very bug that previously made `shutdown_ms`
        // read 0.0 on this scenario.
        let mut staged: Vec<LiteBox> = Vec::with_capacity(N_BOXES);
        for i in 0..N_BOXES {
            let b = rt
                .create(alpine_options(), None)
                .await
                .with_context(|| format!("rt.create #{i}"))?;
            b.start().await.with_context(|| format!("box #{i}.start"))?;
            staged.push(b);
        }

        let t = Instant::now();
        rt.shutdown(None).await.context("rt.shutdown")?;
        let shutdown_ms = t.elapsed().as_secs_f64() * 1000.0;
        drop(staged);

        let mut metrics = BTreeMap::new();
        metrics.insert("shutdown_n_boxes".into(), N_BOXES as f64);
        metrics.insert("shutdown_ms".into(), shutdown_ms);
        Ok(metrics)
    }
}

//! Warm-cache image-pull latency.
//!
//! Counterpart to `throughput-image-pull` (cold-cache, fresh `--home`
//! every iter, headline MB/s). This scenario uses a SHARED home
//! across iterations: the first iter populates the cache, subsequent
//! iters hit boxlite's manifest cache. The headline metric is "how
//! cheap is a no-op pull when the image is already there" — the
//! number folks see when re-running `boxlite pull alpine` after the
//! image is already local.
//!
//! Per iteration:
//!   1. Reuse the shared home (created once per scenario instance).
//!   2. Call `images.pull("alpine:latest")`, time it.
//!
//! First-call pre-warm: with `--runs 1 --warmup 0` (sweep-style
//! single sample), there is no prior iteration to populate the
//! cache, so the lone measured call would be a cold pull —
//! defeating the whole scenario. Track a `prewarmed` flag on the
//! struct; on the first `run_once` we do a hidden cold pull to fill
//! the cache, then run the measured warm pull. The runner's
//! `--warmup` knob still works on top — extra warmup iterations
//! just hit the cache again, harmless.
//!
//! Reports:
//!   * `pull_cached_ms` — pull wall time. Manifest cache hit, so
//!     this should be ms-scale, not seconds.
//!   * `pull_cached_iter` — iteration counter (post-prewarm) for
//!     drift detection across multiple samples.

use super::super::runner::{RunContext, Scenario};
use super::common::build_runtime;
use anyhow::{Context, Result};
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::time::Instant;
use tempfile::TempDir;

const IMAGE: &str = "alpine:latest";

pub struct ImagePullCached {
    home: Option<TempDir>,
    iter: u64,
    prewarmed: bool,
}

impl ImagePullCached {
    pub fn new() -> Self {
        Self {
            home: None,
            iter: 0,
            prewarmed: false,
        }
    }
}

#[async_trait]
impl Scenario for ImagePullCached {
    fn name(&self) -> &str {
        "latency-image-pull-cached"
    }

    async fn run_once(&mut self, ctx: &RunContext) -> Result<BTreeMap<String, f64>> {
        if self.home.is_none() {
            self.home = Some(TempDir::new().context("mkdir image-pull-cached home")?);
        }
        let home_path = self
            .home
            .as_ref()
            .expect("just initialized")
            .path()
            .to_path_buf();
        let rt = build_runtime(ctx.global, home_path)?;

        let images = rt
            .images()
            .context("BoxliteRuntime::images() — no local image manager?")?;

        // Hidden pre-warm cycle on first call: do a cold pull so the
        // manifest + layer cache exists, then fall through to the
        // measured pull which should hit the cache.
        if !self.prewarmed {
            images
                .pull(IMAGE)
                .await
                .with_context(|| format!("pre-warm pull {IMAGE}"))?;
            self.prewarmed = true;
        }

        let start = Instant::now();
        let _image_object = images
            .pull(IMAGE)
            .await
            .with_context(|| format!("pull {IMAGE} (cached)"))?;
        let pull_ms = start.elapsed().as_secs_f64() * 1000.0;
        self.iter += 1;

        let mut metrics = BTreeMap::new();
        metrics.insert("pull_cached_ms".into(), pull_ms);
        metrics.insert("pull_cached_iter".into(), self.iter as f64);
        Ok(metrics)
    }
}

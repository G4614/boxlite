//! Batch clone — `LiteBox::clone_boxes` shares a single base disk
//! copy across N clones, each getting a thin overlay (~64 KB).
//! Distinct from `latency-clone` (which calls `clone_box` once);
//! this scenario tests the optimized batch path that compose
//! workflows use to fan out N variants of one image.
//!
//! Per iteration: clone 10 boxes from a pre-staged 64-MiB source,
//! measure total wall + per-clone amortized cost. The headline
//! number `clone_batch_per_clone_ms` should be MUCH smaller than
//! `latency-clone` — that's the entire point of the batch API.

use super::super::runner::{RunContext, Scenario};
use super::common::{alpine_options, build_runtime};
use anyhow::{Context, Result};
use async_trait::async_trait;
use boxlite::BoxCommand;
use boxlite::runtime::options::CloneOptions;
use futures::StreamExt;
use std::collections::BTreeMap;
use std::time::Instant;
use tempfile::TempDir;

const N: usize = 10;

pub struct CloneBatch {
    home: Option<TempDir>,
    source_id: Option<String>,
    staged: bool,
}

impl CloneBatch {
    pub fn new() -> Self {
        Self {
            home: None,
            source_id: None,
            staged: false,
        }
    }
}

#[async_trait]
impl Scenario for CloneBatch {
    fn name(&self) -> &str {
        "latency-clone-batch-10"
    }

    async fn run_once(&mut self, ctx: &RunContext) -> Result<BTreeMap<String, f64>> {
        if self.home.is_none() {
            self.home = Some(TempDir::new().context("mkdir clone-batch home")?);
        }
        let home_path = self
            .home
            .as_ref()
            .expect("just initialized")
            .path()
            .to_path_buf();
        let rt = build_runtime(ctx.global, home_path)?;

        if !self.staged {
            let mut opts = alpine_options();
            opts.auto_remove = false;
            let src = rt.create(opts, None).await.context("rt.create source")?;
            let src_id = src.id().to_string();
            src.start().await.context("source.start")?;
            let dd = BoxCommand::new("dd").args([
                "if=/dev/zero",
                "of=/tmp/baseline",
                "bs=1M",
                "count=64",
                "conv=fsync",
            ]);
            let mut exec = src.exec(dd).await.context("dd stage")?;
            if let Some(mut s) = exec.stdout() {
                tokio::spawn(async move { while s.next().await.is_some() {} });
            }
            if let Some(mut s) = exec.stderr() {
                tokio::spawn(async move { while s.next().await.is_some() {} });
            }
            let r = exec.wait().await.context("dd stage wait")?;
            if r.exit_code != 0 {
                anyhow::bail!("dd stage exit {}", r.exit_code);
            }
            src.stop().await.context("source.stop")?;
            self.source_id = Some(src_id);
            self.staged = true;
        }

        let source_id = self.source_id.as_ref().expect("staged").clone();
        let src = rt
            .get(&source_id)
            .await
            .context("re-get source")?
            .context("source vanished")?;

        // Unique names per iteration so cleanup of one iteration's
        // clones doesn't collide with the next.
        let names: Vec<String> = (0..N)
            .map(|i| {
                format!(
                    "clone-{}-{}",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_nanos())
                        .unwrap_or(0),
                    i
                )
            })
            .collect();

        let t0 = Instant::now();
        let clones = src
            .clone_boxes(CloneOptions::default(), N, names)
            .await
            .context("clone_boxes")?;
        let batch_ms = t0.elapsed().as_secs_f64() * 1000.0;

        let clone_ids: Vec<String> = clones.iter().map(|c| c.id().to_string()).collect();
        drop(clones);
        for id in clone_ids {
            let _ = rt.remove(&id, true).await;
        }

        let mut metrics = BTreeMap::new();
        metrics.insert("clone_batch_count".into(), N as f64);
        metrics.insert("clone_batch_ms".into(), batch_ms);
        metrics.insert("clone_batch_per_clone_ms".into(), batch_ms / N as f64);
        Ok(metrics)
    }
}

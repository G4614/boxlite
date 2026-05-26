//! N concurrent execs on one box. Complements `stability-exec-
//! loop` (serial). Tests how the guest agent + boxlite-shim
//! handle multiple in-flight exec sessions simultaneously —
//! exposes lock contention in the guest's exec state map,
//! tonic gRPC server fairness, vsock channel multiplexing.
//!
//! Per iteration:
//!   1. Start one alpine box.
//!   2. Spawn N tokio tasks, each fires `boxlite exec sleep 1`
//!      against the same `LiteBox`.
//!   3. Wait for all to complete. Measure aggregate wall +
//!      per-exec p50/p99 (computed inside the iteration since
//!      tail latency under concurrency is the headline signal).

use super::super::runner::{RunContext, Scenario};
use super::common::{BoxGuard, alpine_options, build_runtime};
use anyhow::{Context, Result};
use async_trait::async_trait;
use boxlite::BoxCommand;
use futures::StreamExt;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;
use tempfile::TempDir;

const N: usize = 20;

pub struct ExecParallel {
    home: Option<TempDir>,
}

impl ExecParallel {
    pub fn new() -> Self {
        Self { home: None }
    }
}

#[async_trait]
impl Scenario for ExecParallel {
    fn name(&self) -> &str {
        "stability-exec-parallel"
    }

    async fn run_once(&mut self, ctx: &RunContext) -> Result<BTreeMap<String, f64>> {
        if self.home.is_none() {
            self.home = Some(TempDir::new().context("mkdir exec-parallel home")?);
        }
        let home_path = self
            .home
            .as_ref()
            .expect("just initialized")
            .path()
            .to_path_buf();
        let rt = build_runtime(ctx.global, home_path)?;

        let live = rt
            .create(alpine_options(), None)
            .await
            .context("rt.create(alpine)")?;
        let mut guard = BoxGuard::new(&rt, live.id().to_string());
        live.start().await.context("box.start()")?;

        // Share LiteBox via Arc into the spawn closures. LiteBox
        // is Clone (it's an Arc-backed handle), so this is cheap.
        let live = Arc::new(live);

        let batch_start = Instant::now();
        let mut handles = Vec::with_capacity(N);
        for _ in 0..N {
            let live = Arc::clone(&live);
            handles.push(tokio::spawn(async move {
                let cmd = BoxCommand::new("sleep").args(["1"]);
                let started = Instant::now();
                let mut exec = live.exec(cmd).await?;
                if let Some(mut s) = exec.stdout() {
                    tokio::spawn(async move { while s.next().await.is_some() {} });
                }
                if let Some(mut s) = exec.stderr() {
                    tokio::spawn(async move { while s.next().await.is_some() {} });
                }
                exec.wait().await?;
                Ok::<f64, anyhow::Error>(started.elapsed().as_secs_f64() * 1000.0)
            }));
        }
        let mut times = Vec::with_capacity(N);
        for h in handles {
            match h.await {
                Ok(Ok(t)) => times.push(t),
                Ok(Err(e)) => anyhow::bail!("concurrent exec failed: {e:#}"),
                Err(je) => anyhow::bail!("task join: {je}"),
            }
        }
        let batch_ms = batch_start.elapsed().as_secs_f64() * 1000.0;

        // Take Arc out so .stop() can borrow LiteBox directly.
        let live = Arc::try_unwrap(live)
            .ok()
            .context("LiteBox still has outstanding Arc refs")?;
        live.stop().await.context("box.stop()")?;
        guard.disarm();

        times.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let p50 = times.get(times.len() / 2).copied().unwrap_or(0.0);
        let p99_idx = ((times.len() as f64 * 0.99).ceil() as usize).saturating_sub(1);
        let p99 = times.get(p99_idx).copied().unwrap_or(0.0);
        let max = times.last().copied().unwrap_or(0.0);

        let mut metrics = BTreeMap::new();
        metrics.insert("exec_parallel_count".into(), N as f64);
        metrics.insert("exec_parallel_batch_ms".into(), batch_ms);
        metrics.insert("exec_parallel_p50_ms".into(), p50);
        metrics.insert("exec_parallel_p99_ms".into(), p99);
        metrics.insert("exec_parallel_max_ms".into(), max);
        Ok(metrics)
    }
}

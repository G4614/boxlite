//! N consecutive `boxlite exec` calls on a single running box —
//! catches guest agent / shim per-exec leaks that
//! `stability-churn` (which recreates the box every cycle) and
//! `stability-soak` (idle, no execs) both miss.
//!
//! Per iteration:
//!   1. Start one alpine box.
//!   2. Record host fd count.
//!   3. In a loop, fire up to N execs of a trivial in-box command
//!      (`echo`), wait each one to exit. Time the loop. If any
//!      exec / wait errors out, capture the failing index and
//!      break — the scenario still produces metrics so the
//!      progression is visible in reports.
//!   4. Record host fd count again.
//!   5. Snapshot `BoxMetrics` once at the end (guest agent's
//!      view).
//!
//! Reports:
//!   * `exec_loop_count` — N (target).
//!   * `exec_completed_count` — execs that actually succeeded.
//!     Less than `exec_loop_count` indicates a boxlite-side
//!     regression in the exec subsystem. A historical observed
//!     boundary (boxlite 0.9.5 alpine x86_64) is exec #247 →
//!     `InitReady`/`IntermediateReady(0)` mismatch in the guest
//!     init pipeline; a future change pushing that lower flags
//!     a regression.
//!   * `exec_mean_ms`, `exec_max_ms` — over successful execs.
//!   * `exec_fd_delta_count` — host-side fd delta over the loop.
//!   * `exec_rss_end_bytes` — guest RSS after the loop completes.

use super::super::runner::{RunContext, Scenario};
use super::common::{BoxGuard, alpine_options, build_runtime};
use anyhow::{Context, Result};
use async_trait::async_trait;
use boxlite::BoxCommand;
use futures::StreamExt;
use std::collections::BTreeMap;
use std::time::Instant;
use tempfile::TempDir;

const N: usize = 500;

pub struct ExecLoop {
    home: Option<TempDir>,
}

impl ExecLoop {
    pub fn new() -> Self {
        Self { home: None }
    }
}

#[async_trait]
impl Scenario for ExecLoop {
    fn name(&self) -> &str {
        "stability-exec-loop"
    }

    async fn run_once(&mut self, ctx: &RunContext) -> Result<BTreeMap<String, f64>> {
        if self.home.is_none() {
            self.home = Some(TempDir::new().context("mkdir exec-loop home")?);
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

        let fd_before = host_fd_count();
        let mut per_exec = Vec::with_capacity(N);

        let mut first_failure: Option<(usize, String)> = None;
        for i in 0..N {
            let cmd = BoxCommand::new("echo").args(["hi"]);
            let start = Instant::now();
            let mut exec = match live.exec(cmd).await {
                Ok(e) => e,
                Err(e) => {
                    first_failure = Some((i, format!("exec spawn: {e:#}")));
                    break;
                }
            };
            if let Some(mut s) = exec.stdout() {
                tokio::spawn(async move { while s.next().await.is_some() {} });
            }
            if let Some(mut s) = exec.stderr() {
                tokio::spawn(async move { while s.next().await.is_some() {} });
            }
            if let Err(e) = exec.wait().await {
                first_failure = Some((i, format!("exec wait: {e:#}")));
                break;
            }
            per_exec.push(start.elapsed().as_secs_f64() * 1000.0);
        }
        if let Some((i, why)) = &first_failure {
            eprintln!("stability-exec-loop: exec #{i} failed: {why}");
        }

        let fd_after = host_fd_count();
        let final_metrics = live.metrics().await.context("BoxMetrics post-loop")?;

        live.stop().await.context("box.stop()")?;
        guard.disarm();

        let mut metrics = BTreeMap::new();
        metrics.insert("exec_loop_count".into(), N as f64);
        metrics.insert("exec_completed_count".into(), per_exec.len() as f64);
        if !per_exec.is_empty() {
            let mean = per_exec.iter().copied().sum::<f64>() / per_exec.len() as f64;
            let max = per_exec.iter().copied().fold(f64::NEG_INFINITY, f64::max);
            metrics.insert("exec_mean_ms".into(), mean);
            metrics.insert("exec_max_ms".into(), max);
        }
        if let (Some(before), Some(after)) = (fd_before, fd_after) {
            metrics.insert("exec_fd_delta_count".into(), after as f64 - before as f64);
        }
        if let Some(rss) = final_metrics.memory_bytes() {
            metrics.insert("exec_rss_end_bytes".into(), rss as f64);
        }
        Ok(metrics)
    }
}

fn host_fd_count() -> Option<usize> {
    Some(std::fs::read_dir("/proc/self/fd").ok()?.count())
}

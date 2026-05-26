//! Per-exec latency over the REST/WebSocket exec channel. The
//! in-process equivalent is `stability-exec-loop`, which reports
//! ~9 ms/exec on alpine. Routing the same op through `boxlite serve`
//! adds: HTTP POST to open the execution, WebSocket upgrade,
//! tokio-tungstenite frame parsing, axum middleware, axum-tower
//! authn checks, the REST-side `ExecBackend`'s message pump. Each
//! is a few hundred µs to a few ms, and together they're the floor
//! number for SDK callers running `exec` in a tight loop.
//!
//! Per iteration:
//!   1. Spawn `boxlite serve` child.
//!   2. Build a REST runtime, create + start one box once per
//!      iteration (re-using boxes across iterations would conflate
//!      the per-exec measurement with shared-state effects).
//!   3. Run `EXECS` echoes through the REST exec path back-to-back,
//!      timing each.
//!   4. Tear the box down, drop the server.
//!
//! Reports:
//!   * `ws_exec_count` — N (==EXECS).
//!   * `ws_exec_mean_ms`, `ws_exec_max_ms`, `ws_exec_p99_ms`.
//!
//! Delta against `stability-exec-loop` ≈ the REST/WS framing tax.

use super::super::runner::{RunContext, Scenario};
use super::common::ServeChild;
use anyhow::{Context, Result};
use async_trait::async_trait;
use boxlite::runtime::options::RootfsSpec;
use boxlite::{BoxCommand, BoxOptions, BoxliteRestOptions, BoxliteRuntime, LiteBox};
use futures::StreamExt;
use std::collections::BTreeMap;
use std::time::Instant;

const EXECS: usize = 100;

pub struct WsExec;

impl WsExec {
    pub fn new() -> Self {
        Self
    }
}

async fn run_exec(live: &LiteBox) -> Result<f64> {
    let cmd = BoxCommand::new("echo").args(["hi"]);
    let t = Instant::now();
    let mut exec = live.exec(cmd).await.context("live.exec(echo) over REST")?;
    if let Some(mut s) = exec.stdout() {
        tokio::spawn(async move { while s.next().await.is_some() {} });
    }
    if let Some(mut s) = exec.stderr() {
        tokio::spawn(async move { while s.next().await.is_some() {} });
    }
    let _ = exec.wait().await.context("exec.wait() over REST")?;
    Ok(t.elapsed().as_secs_f64() * 1000.0)
}

fn nearest_rank(sorted: &[f64], p: u32) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let n = sorted.len();
    let rank = ((p as f64 / 100.0) * n as f64).ceil() as usize;
    let idx = rank.clamp(1, n) - 1;
    sorted[idx]
}

#[async_trait]
impl Scenario for WsExec {
    fn name(&self) -> &str {
        "latency-ws-exec"
    }

    async fn run_once(&mut self, ctx: &RunContext) -> Result<BTreeMap<String, f64>> {
        let server = ServeChild::spawn("ws-exec", &ctx.global.registry).await?;

        let rest_opts = BoxliteRestOptions::new(&server.url);
        let rt = BoxliteRuntime::rest(rest_opts).context("BoxliteRuntime::rest")?;

        let opts = BoxOptions {
            rootfs: RootfsSpec::Image("alpine:latest".into()),
            auto_remove: true,
            ..Default::default()
        };
        let live = rt.create(opts, None).await.context("rt.create over REST")?;
        live.start().await.context("box.start over REST")?;

        let mut samples: Vec<f64> = Vec::with_capacity(EXECS);
        for _ in 0..EXECS {
            samples.push(run_exec(&live).await?);
        }

        let _ = live.stop().await;

        let mean = samples.iter().copied().sum::<f64>() / samples.len() as f64;
        let max = samples.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let mut sorted = samples.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

        let mut metrics = BTreeMap::new();
        metrics.insert("ws_exec_count".into(), EXECS as f64);
        metrics.insert("ws_exec_mean_ms".into(), mean);
        metrics.insert("ws_exec_p50_ms".into(), nearest_rank(&sorted, 50));
        metrics.insert("ws_exec_p99_ms".into(), nearest_rank(&sorted, 99));
        metrics.insert("ws_exec_max_ms".into(), max);

        drop(server);
        Ok(metrics)
    }
}

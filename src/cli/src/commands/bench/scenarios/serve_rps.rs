//! `boxlite serve` RPS scenario.
//!
//! `throughput-serve-rps` spawns `boxlite serve` as a child process,
//! waits for it to bind, then hammers `GET /v1/config` from a pool
//! of concurrent reqwest tasks for `WINDOW_SECS` seconds and reports
//! the achieved RPS.
//!
//! `/v1/config` is the right hammer target: it's unauthenticated (no
//! API-key dance), doesn't touch the box DB (no SQLite write
//! contention), and is the smallest serializable JSON response the
//! server hands out. So the rate this scenario measures is the
//! axum + tower + serde overhead per request — the *floor* for any
//! other endpoint. A regression here implies every other endpoint
//! got slower in lockstep.
//!
//! Lifecycle inside one iteration:
//!   1. Host probes a free port.
//!   2. Spawn `boxlite serve --host 127.0.0.1 --port <N>` as a child
//!      with `kill_on_drop(true)` so a panic teardown can't leak it.
//!      The child uses a fresh `TempDir` home so it doesn't trip
//!      over our parent process's `--home`.
//!   3. Poll `/v1/config` until 200 OK (or fail at READY_TIMEOUT).
//!   4. Spawn CONCURRENCY worker tasks, each looping a GET against
//!      `/v1/config` until `WINDOW_SECS` elapse. Successes and
//!      failures are counted into atomics.
//!   5. Send SIGTERM to the child + wait for it.
//!
//! Reports:
//!   * `serve_rps` — successful GETs per second.
//!   * `serve_success_count`  — raw 200s in the window.
//!   * `serve_error_count` — non-200s + transport errors.
//!   * `serve_concurrency` — `CONCURRENCY` const captured into the
//!     report so cross-version diffs can't compare different load
//!     profiles silently.

use super::super::runner::{RunContext, Scenario};
use anyhow::{Context, Result};
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::net::TcpListener as StdTcpListener;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tokio::process::Command;

const WINDOW_SECS: u64 = 5;
const CONCURRENCY: usize = 16;
const READY_TIMEOUT: Duration = Duration::from_secs(10);
const HAMMER_TIMEOUT_PER_REQ: Duration = Duration::from_secs(5);

pub struct ServeRps;

impl ServeRps {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Scenario for ServeRps {
    fn name(&self) -> &str {
        "throughput-serve-rps"
    }

    async fn run_once(&mut self, _ctx: &RunContext) -> Result<BTreeMap<String, f64>> {
        // Probe a free port. Same TOCTOU race window as the other
        // scenarios that do this — acceptable; on the very rare
        // loss the child `boxlite serve` will fail to bind and the
        // ready-poll will time out, surfacing a real error.
        let port = {
            let probe = StdTcpListener::bind(("127.0.0.1", 0)).context("probe a free host port")?;
            probe
                .local_addr()
                .map(|addr| addr.port())
                .context("read probed addr")?
        };

        // Fresh home for the child server so its sqlite/locks don't
        // collide with anything else running on this host. TempDir
        // drop tears it down at scope exit.
        let home = TempDir::new().context("mkdir serve-rps child home")?;

        let bin = std::env::current_exe().context("locate current boxlite binary")?;
        let mut child = Command::new(&bin)
            .arg("--home")
            .arg(home.path())
            .args(["serve", "--host", "127.0.0.1", "--port", &port.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .context("spawn boxlite serve child")?;

        // Poll until the server's `/v1/config` answers 200.
        let url = format!("http://127.0.0.1:{port}/v1/config");
        let client = reqwest::Client::builder()
            .timeout(HAMMER_TIMEOUT_PER_REQ)
            .build()
            .context("build reqwest client")?;
        let ready_at = Instant::now() + READY_TIMEOUT;
        loop {
            if Instant::now() > ready_at {
                let _ = child.start_kill();
                let _ = child.wait().await;
                anyhow::bail!(
                    "boxlite serve never answered 200 on /v1/config within {}s",
                    READY_TIMEOUT.as_secs()
                );
            }
            match client.get(&url).send().await {
                Ok(r) if r.status().is_success() => break,
                _ => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }

        // Hammer phase. Atomics over CONCURRENCY workers; each
        // loops GETs until the global stop instant.
        let success = Arc::new(AtomicU64::new(0));
        let errors = Arc::new(AtomicU64::new(0));
        let stop_at = Instant::now() + Duration::from_secs(WINDOW_SECS);
        let hammer_start = Instant::now();
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
        let elapsed = hammer_start.elapsed().as_secs_f64();

        // Tear down the child. SIGKILL (kill()) over SIGTERM is OK
        // here — we don't care about a graceful shutdown for a
        // throwaway server.
        let _ = child.start_kill();
        let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;

        let succ = success.load(Ordering::Relaxed);
        let err = errors.load(Ordering::Relaxed);

        let mut metrics = BTreeMap::new();
        if elapsed > 0.0 {
            metrics.insert("serve_rps".into(), succ as f64 / elapsed);
        }
        metrics.insert("serve_success_count".into(), succ as f64);
        metrics.insert("serve_error_count".into(), err as f64);
        metrics.insert("serve_concurrency".into(), CONCURRENCY as f64);
        Ok(metrics)
    }
}

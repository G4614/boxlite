//! TCP connections-per-second establish rate. Tests how fast
//! gvproxy can accept + complete new TCP handshakes (distinct
//! from throughput, which is bytes-per-second on already-open
//! connections).
//!
//! Architecture:
//!   1. Probe free port; box created with `-p <host>:9999`.
//!   2. In-box: `sh -c 'while true; do nc -l -p 9999 -q 0; done'`
//!      — accepts a connection, closes it immediately (`-q 0`).
//!      Loop relistens. busybox nc's `-q 0` makes it exit right
//!      after the peer EOFs, which works in our connect-then-
//!      shutdown_write client flow.
//!   3. Host: tight tokio loop of `TcpStream::connect` → drop,
//!      counts successes for `WINDOW_SECS`.
//!
//! `nc` per-connection respawn overhead in-box probably caps
//! this around 1000 CPS; the regression signal is changes in the
//! rate, not the absolute number.
//!
//! Reports:
//!   * `tcp_cps_count_per_sec` — successful connects per second.
//!   * `tcp_cps_total_count` — total in the window.
//!   * `tcp_cps_errors_count` — failed connects.

use super::super::runner::{RunContext, Scenario};
use super::common::{BoxGuard, build_runtime};
use anyhow::{Context, Result};
use async_trait::async_trait;
use boxlite::runtime::options::{PortProtocol, PortSpec, RootfsSpec};
use boxlite::{BoxCommand, BoxOptions};
use futures::StreamExt;
use std::collections::BTreeMap;
use std::net::TcpListener as StdTcpListener;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tokio::net::TcpStream;

const WINDOW_SECS: u64 = 3;
const GUEST_PORT: u16 = 9999;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

pub struct TcpCps {
    home: Option<TempDir>,
}

impl TcpCps {
    pub fn new() -> Self {
        Self { home: None }
    }
}

#[async_trait]
impl Scenario for TcpCps {
    fn name(&self) -> &str {
        "throughput-tcp-cps"
    }

    async fn run_once(&mut self, ctx: &RunContext) -> Result<BTreeMap<String, f64>> {
        if self.home.is_none() {
            self.home = Some(TempDir::new().context("mkdir tcp-cps home")?);
        }
        let home_path = self
            .home
            .as_ref()
            .expect("just initialized")
            .path()
            .to_path_buf();
        let rt = build_runtime(ctx.global, home_path)?;

        let host_port = {
            let probe = StdTcpListener::bind(("0.0.0.0", 0)).context("probe free port")?;
            probe
                .local_addr()
                .map(|a| a.port())
                .context("read probed addr")?
        };

        let opts = BoxOptions {
            rootfs: RootfsSpec::Image("alpine:latest".into()),
            auto_remove: true,
            ports: vec![PortSpec {
                host_port: Some(host_port),
                guest_port: GUEST_PORT,
                protocol: PortProtocol::Tcp,
                host_ip: None,
            }],
            ..Default::default()
        };
        let live = rt.create(opts, None).await.context("rt.create(alpine)")?;
        let mut guard = BoxGuard::new(&rt, live.id().to_string());
        live.start().await.context("box.start()")?;

        // Loop nc indefinitely — each invocation accepts one
        // connection then exits, the `while true` respawns.
        // `-q 0` quits immediately after stdin EOF; combined with
        // our host-side `drop(stream)` (which sends RST/FIN),
        // gives the fastest possible turnaround.
        let nc_loop =
            format!("while true; do nc -l -p {GUEST_PORT} -q 0 < /dev/null > /dev/null 2>&1; done");
        let listener_cmd = BoxCommand::new("sh").args(["-c", &nc_loop]);
        let mut listener_exec = live.exec(listener_cmd).await.context("box.exec(nc loop)")?;
        if let Some(mut s) = listener_exec.stdout() {
            tokio::spawn(async move { while s.next().await.is_some() {} });
        }
        if let Some(mut s) = listener_exec.stderr() {
            tokio::spawn(async move { while s.next().await.is_some() {} });
        }

        tokio::time::sleep(Duration::from_millis(800)).await;

        let stop_at = Instant::now() + Duration::from_secs(WINDOW_SECS);
        let mut ok: u64 = 0;
        let mut err: u64 = 0;
        let started = Instant::now();
        while Instant::now() < stop_at {
            match tokio::time::timeout(
                CONNECT_TIMEOUT,
                TcpStream::connect(("127.0.0.1", host_port)),
            )
            .await
            {
                Ok(Ok(stream)) => {
                    ok += 1;
                    drop(stream);
                }
                _ => {
                    err += 1;
                }
            }
        }
        let elapsed = started.elapsed().as_secs_f64();

        let _ = listener_exec.kill().await;
        let _ = tokio::time::timeout(Duration::from_secs(3), listener_exec.wait()).await;

        live.stop().await.context("box.stop()")?;
        guard.disarm();

        let mut metrics = BTreeMap::new();
        if elapsed > 0.0 {
            metrics.insert("tcp_cps_count_per_sec".into(), ok as f64 / elapsed);
        }
        metrics.insert("tcp_cps_total_count".into(), ok as f64);
        metrics.insert("tcp_cps_errors_count".into(), err as f64);
        Ok(metrics)
    }
}

//! Reverse-direction iperf3 — measures guest→host throughput
//! through gvproxy's outbound NAT path.
//!
//! Distinct from `throughput-net-iperf3` (host→guest via port
//! forward): outbound from the guest uses a different gvproxy
//! code path entirely — guest packets hit eth0, gvproxy's
//! virtualnetwork NATs them to the host loopback, and the host
//! iperf3 server receives. Regressions in the outbound NAT path
//! (e.g., a per-connection map that doesn't get freed) only show
//! up here.
//!
//! Architecture:
//!   1. Host: start `iperf3 -s -1 -p <free_port> --bind 0.0.0.0
//!      -D` (daemon mode, bound to all interfaces so the guest
//!      can reach via gvproxy's virtual host address).
//!   2. Box: `iperf3 -c <gvproxy_host_ip> -p <host_port> -J -t N`.
//!      gvproxy translates the guest's connection to the host
//!      automatically — no `-p` mapping needed since this is the
//!      OUTBOUND direction.
//!   3. Parse iperf3 JSON from the box's stdout.
//!
//! `gvproxy_host_ip` is `192.168.127.254` — gvproxy's
//! "host.docker.internal" equivalent. See `src/boxlite/src/net/
//! gvproxy/config.rs` for the constants.
//!
//! Reports: same shape as `throughput-net-iperf3` (`iperf3_*`)
//! but prefixed `egress_` to distinguish.

use super::super::runner::{RunContext, Scenario, TeardownContext};
use super::common::{BoxGuard, alpine_options, build_runtime};
use anyhow::{Context, Result};
use async_trait::async_trait;
use boxlite::BoxCommand;
use futures::StreamExt;
use serde_json::Value;
use std::collections::BTreeMap;
use std::net::TcpListener as StdTcpListener;
use std::process::Stdio;
use std::time::Duration;
use tempfile::TempDir;
use tokio::process::Command as TokioCommand;

const TRANSFER_SECS: u64 = 5;
/// gvproxy's virtual "host IP" — guests reach the host via this
/// address. Documented in `src/boxlite/src/net/gvproxy/config.rs`.
const GVPROXY_HOST_IP: &str = "192.168.127.254";
const SERVER_READY_WAIT: Duration = Duration::from_millis(500);

pub struct NetIperf3Egress {
    home: Option<TempDir>,
    iperf3_installed: bool,
    /// Track host ports we've spawned `iperf3 -s -D` daemons on so
    /// teardown can pkill any lingerers if the `-1` self-exit didn't
    /// fire (in-box client errored mid-handshake, etc.).
    daemon_ports: Vec<u16>,
}

impl NetIperf3Egress {
    pub fn new() -> Self {
        Self {
            home: None,
            iperf3_installed: false,
            daemon_ports: Vec::new(),
        }
    }
}

#[async_trait]
impl Scenario for NetIperf3Egress {
    fn name(&self) -> &str {
        "throughput-net-iperf3-egress"
    }

    async fn run_once(&mut self, ctx: &RunContext) -> Result<BTreeMap<String, f64>> {
        if which_iperf3().is_none() {
            eprintln!("SKIP throughput-net-iperf3-egress: host iperf3 binary not found.");
            let mut out = BTreeMap::new();
            out.insert("iperf3_egress_skipped".into(), 1.0);
            return Ok(out);
        }

        if self.home.is_none() {
            self.home = Some(TempDir::new().context("mkdir net-iperf3-egress home")?);
        }
        let home_path = self
            .home
            .as_ref()
            .expect("just initialized")
            .path()
            .to_path_buf();
        let rt = build_runtime(ctx.global, home_path)?;

        let host_port = {
            let probe = StdTcpListener::bind(("0.0.0.0", 0)).context("probe free host port")?;
            probe
                .local_addr()
                .map(|a| a.port())
                .context("read probed addr")?
        };

        // Start host iperf3 server in daemon mode (`-D` exits the
        // foreground after the daemon child is up). `-1` makes the
        // daemon child exit after a single connection, so we don't
        // need to track + kill it.
        let server_start = TokioCommand::new("iperf3")
            .args([
                "-s",
                "-D",
                "-1",
                "-p",
                &host_port.to_string(),
                "--bind",
                "0.0.0.0",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .context("spawn host iperf3 -s -D")?;
        if !server_start.success() {
            anyhow::bail!(
                "host iperf3 -s -D failed to launch (exit {:?})",
                server_start.code()
            );
        }
        // Stash the port so teardown can pkill the daemon if `-1`
        // didn't get a clean client (the failure-path leak).
        self.daemon_ports.push(host_port);

        let live = rt
            .create(alpine_options(), None)
            .await
            .context("rt.create(alpine)")?;
        let mut guard = BoxGuard::new(&rt, live.id().to_string());
        live.start().await.context("box.start()")?;

        if !self.iperf3_installed {
            let install = BoxCommand::new("apk").args(["add", "--no-cache", "iperf3"]);
            let mut exec = live.exec(install).await.context("apk add iperf3")?;
            if let Some(mut s) = exec.stdout() {
                tokio::spawn(async move { while s.next().await.is_some() {} });
            }
            if let Some(mut s) = exec.stderr() {
                tokio::spawn(async move { while s.next().await.is_some() {} });
            }
            let r = exec.wait().await.context("apk add iperf3 wait")?;
            if r.exit_code != 0 {
                anyhow::bail!("apk add iperf3 failed (exit {})", r.exit_code);
            }
            self.iperf3_installed = true;
        }

        // Brief settle so the daemonized host server is actually
        // accept()ing by the time the guest client tries to
        // connect (the iperf3 -D ack returns before listen() in
        // some builds).
        tokio::time::sleep(SERVER_READY_WAIT).await;

        let client_cmd = BoxCommand::new("iperf3").args([
            "-c",
            GVPROXY_HOST_IP,
            "-p",
            &host_port.to_string(),
            "-J",
            "-t",
            &TRANSFER_SECS.to_string(),
        ]);
        let mut client_exec = live.exec(client_cmd).await.context("box.exec(iperf3 -c)")?;
        let mut stdout = client_exec.stdout().expect("stdout handle");
        if let Some(mut s) = client_exec.stderr() {
            tokio::spawn(async move { while s.next().await.is_some() {} });
        }

        // Bounded: `-t` should cap iperf3 at TRANSFER_SECS, but rare
        // gvproxy or guest-agent races have wedged the stdout pump
        // in the field. Cap the whole drain+wait so a hang fails the
        // iteration instead of consuming the entire sweep slot.
        let drain_budget = Duration::from_secs(TRANSFER_SECS + 25);
        let json_buf = match tokio::time::timeout(drain_budget, async {
            let mut buf = String::new();
            while let Some(chunk) = stdout.next().await {
                buf.push_str(&chunk);
            }
            buf
        })
        .await
        {
            Ok(buf) => buf,
            Err(_) => anyhow::bail!(
                "in-box iperf3 client stdout drain hung > {}s",
                drain_budget.as_secs()
            ),
        };
        let r = match tokio::time::timeout(Duration::from_secs(10), client_exec.wait()).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => anyhow::bail!("iperf3 client wait error: {e:#}"),
            Err(_) => anyhow::bail!("in-box iperf3 client wait hung > 10s"),
        };
        if r.exit_code != 0 {
            anyhow::bail!(
                "in-box iperf3 client exited non-zero ({}); stdout:\n{json_buf}",
                r.exit_code
            );
        }

        let metrics = parse_iperf3_json(&json_buf)
            .with_context(|| format!("parse iperf3 JSON; raw was:\n{json_buf}"))?;

        live.stop().await.context("box.stop()")?;
        guard.disarm();
        Ok(metrics)
    }

    async fn teardown(&mut self, _ctx: &TeardownContext<'_>) -> Result<()> {
        // `iperf3 -s -D -1` self-exits after one client session. The
        // leak path is when the in-box client errors mid-handshake
        // and the daemon never gets a clean disconnect — it then
        // waits forever. Match by `-p <port>` since iperf3's process
        // name is just "iperf3" and `pgrep` sees the full argv.
        for port in self.daemon_ports.drain(..) {
            let pattern = format!("iperf3 .*-p {port}");
            let _ = std::process::Command::new("pkill")
                .args(["-f", &pattern])
                .status();
        }
        Ok(())
    }
}

fn which_iperf3() -> Option<std::path::PathBuf> {
    let path_var = std::env::var("PATH").ok()?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join("iperf3");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Same iperf3 JSON schema as the host-side scenario; we just
/// rename the metric keys to `egress_iperf3_*` so they don't
/// collide on cross-version comparisons.
fn parse_iperf3_json(text: &str) -> Result<BTreeMap<String, f64>> {
    let root: Value = serde_json::from_str(text).context("iperf3 JSON parse")?;
    let end = root.get("end").context(".end missing")?;
    let sum_recv = end
        .get("sum_received")
        .context(".end.sum_received missing")?;
    let bps = sum_recv
        .get("bits_per_second")
        .and_then(|v| v.as_f64())
        .context(".end.sum_received.bits_per_second missing")?;
    let dur_s = sum_recv
        .get("seconds")
        .and_then(|v| v.as_f64())
        .context(".end.sum_received.seconds missing")?;
    let retrans = end
        .get("streams")
        .and_then(|s| s.as_array())
        .and_then(|a| a.first())
        .and_then(|s| s.get("sender"))
        .and_then(|s| s.get("retransmits"))
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let mut out = BTreeMap::new();
    out.insert("egress_iperf3_bits_per_sec".into(), bps);
    out.insert("egress_iperf3_retransmits_count".into(), retrans);
    out.insert("egress_iperf3_duration_secs".into(), dur_s);
    Ok(out)
}

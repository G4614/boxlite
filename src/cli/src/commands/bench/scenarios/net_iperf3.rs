//! In-box `iperf3` server + host client → gvproxy throughput measurement.
//!
//! Differs from `throughput-net-tcp-sink` in that it reports the
//! iperf3 protocol's bandwidth in proper Mbps with kernel-level
//! per-second sampling, and exposes retransmits — signals the
//! raw-bytes-through-nc approach cannot give.
//!
//! Architecture:
//!   1. Host probes a free port.
//!   2. Box created with `-p <host>:5201` (iperf3's default).
//!   3. In-box: `apk add iperf3` (one-time per scenario instance,
//!      amortized across iterations because the home is shared and
//!      the COW carries the install).
//!   4. In-box: `iperf3 -s -1 -p 5201` (single-connection server,
//!      exits after the client disconnects).
//!   5. Host: `iperf3 -c 127.0.0.1 -p <host> -J -t <secs>`
//!      (JSON output, fixed duration). Captures stdout for parsing.
//!   6. Parse iperf3 JSON: `end.sum_received.bits_per_second` is the
//!      headline number; `end.streams[0].sender.retransmits` is the
//!      regression-worthy tail signal.
//!
//! Reported metrics:
//!   * `iperf3_bits_per_sec` — `_per_sec` so higher-is-better flips
//!     automatically (units are bps; mb/s readable as / 1e6).
//!   * `iperf3_retransmits_count` — TCP retransmits the sender saw.
//!     Healthy on a sane gvproxy = 0; sustained non-zero indicates a
//!     real loss path that nc-sink wouldn't catch.
//!   * `iperf3_duration_secs` — actual transfer wall time (iperf3
//!     reports its own).

use super::super::runner::{RunContext, Scenario};
use super::common::{BoxGuard, build_runtime};
use anyhow::{Context, Result};
use async_trait::async_trait;
use boxlite::BoxCommand;
use boxlite::BoxOptions;
use boxlite::runtime::options::{PortProtocol, PortSpec, RootfsSpec};
use futures::StreamExt;
use serde_json::Value;
use std::collections::BTreeMap;
use std::net::TcpListener as StdTcpListener;
use std::process::Stdio;
use std::time::Duration;
use tempfile::TempDir;
use tokio::process::Command as TokioCommand;

const TRANSFER_SECS: u64 = 5;
const GUEST_PORT: u16 = 5201;
/// How long to wait for the in-box `iperf3 -s` to bind before the
/// host client fires. Smaller than nc-sink's because iperf3's start
/// is more deterministic.
const SERVER_READY_WAIT: Duration = Duration::from_millis(800);

pub struct NetIperf3 {
    home: Option<TempDir>,
    iperf3_installed: bool,
}

impl NetIperf3 {
    pub fn new() -> Self {
        Self {
            home: None,
            iperf3_installed: false,
        }
    }
}

#[async_trait]
impl Scenario for NetIperf3 {
    fn name(&self) -> &str {
        "throughput-net-iperf3"
    }

    async fn run_once(&mut self, ctx: &RunContext) -> Result<BTreeMap<String, f64>> {
        // Host iperf3 is a hard prerequisite. Skip cleanly (with a
        // marker metric so the report shows the scenario ran but
        // produced no real data) rather than crashing.
        if which_iperf3().is_none() {
            eprintln!(
                "SKIP throughput-net-iperf3: host iperf3 binary not found. \
                 `apt-get install -y iperf3` (or equivalent) to enable."
            );
            let mut out = BTreeMap::new();
            out.insert("iperf3_skipped".into(), 1.0);
            return Ok(out);
        }

        if self.home.is_none() {
            self.home = Some(TempDir::new().context("mkdir net-iperf3 home")?);
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

        // One-time iperf3 install. Cached on the COW for subsequent
        // iterations (shared `--home`).
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

        // Start the server inside the box. `-1` makes it exit after a
        // single client disconnects, so we don't need an explicit
        // kill.
        let server_cmd =
            BoxCommand::new("iperf3").args(["-s", "-1", "-p", &GUEST_PORT.to_string()]);
        let mut server_exec = live.exec(server_cmd).await.context("box.exec(iperf3 -s)")?;
        if let Some(mut s) = server_exec.stdout() {
            tokio::spawn(async move { while s.next().await.is_some() {} });
        }
        if let Some(mut s) = server_exec.stderr() {
            tokio::spawn(async move { while s.next().await.is_some() {} });
        }

        tokio::time::sleep(SERVER_READY_WAIT).await;

        // Host-side client. `-J` = JSON output; `-t` = duration.
        // Bounded explicitly: `-t` should cap the run, but rare
        // gvproxy state-leak races can wedge the control channel and
        // we don't want a sweep to lose its entire slot to one hang.
        let child = TokioCommand::new("iperf3")
            .args([
                "-c",
                "127.0.0.1",
                "-p",
                &host_port.to_string(),
                "-J",
                "-t",
                &TRANSFER_SECS.to_string(),
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("spawn host iperf3 client")?;
        let client_budget = Duration::from_secs(TRANSFER_SECS + 25);
        let client = match tokio::time::timeout(client_budget, child.wait_with_output()).await {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => anyhow::bail!("iperf3 client wait error: {e:#}"),
            Err(_) => anyhow::bail!("iperf3 client hung > {}s", client_budget.as_secs()),
        };

        if !client.status.success() {
            let stderr = String::from_utf8_lossy(&client.stderr);
            anyhow::bail!(
                "host iperf3 client exited non-zero ({:?}); stderr:\n{stderr}",
                client.status.code()
            );
        }
        let json_text = String::from_utf8_lossy(&client.stdout);
        let metrics = parse_iperf3_json(&json_text)
            .with_context(|| format!("parse iperf3 JSON; raw stdout was:\n{json_text}"))?;

        // iperf3 -s -1 should have exited already; bound the wait
        // anyway so a stuck server can't hang the iteration.
        let _ = tokio::time::timeout(Duration::from_secs(3), server_exec.wait()).await;

        live.stop().await.context("box.stop()")?;
        guard.disarm();
        Ok(metrics)
    }
}

fn which_iperf3() -> Option<std::path::PathBuf> {
    use std::env;
    let path_var = env::var("PATH").ok()?;
    for dir in env::split_paths(&path_var) {
        let candidate = dir.join("iperf3");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Extract our headline metrics from iperf3's `-J` JSON.
/// Schema (per iperf3 docs):
///   `.end.sum_received.bits_per_second`  — headline throughput
///   `.end.streams[0].sender.retransmits` — TCP retransmits seen
///   `.end.sum_received.seconds`          — actual transfer duration
fn parse_iperf3_json(text: &str) -> Result<BTreeMap<String, f64>> {
    let root: Value = serde_json::from_str(text).context("iperf3 JSON parse")?;
    let end = root.get("end").context(".end missing in iperf3 output")?;
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
    out.insert("iperf3_bits_per_sec".into(), bps);
    out.insert("iperf3_retransmits_count".into(), retrans);
    out.insert("iperf3_duration_secs".into(), dur_s);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pin the iperf3 JSON parser against the schema shape iperf3
    /// 3.x emits. Catches a future renamed-field regression rather
    /// than the scenario silently reporting 0 bps.
    #[test]
    fn parse_iperf3_json_extracts_headline_metrics() {
        let sample = r#"{
            "end": {
                "streams": [{
                    "sender": { "retransmits": 12 }
                }],
                "sum_received": {
                    "seconds": 5.000123,
                    "bits_per_second": 1234567890.5
                }
            }
        }"#;
        let m = parse_iperf3_json(sample).expect("parse");
        assert_eq!(m["iperf3_bits_per_sec"], 1234567890.5);
        assert_eq!(m["iperf3_duration_secs"], 5.000123);
        assert_eq!(m["iperf3_retransmits_count"], 12.0);
    }

    /// retransmits defaults to 0.0 when absent — some iperf3 builds
    /// strip the field for clean runs. Test pins the silent default
    /// behavior so we don't accidentally start erroring on healthy
    /// runs.
    #[test]
    fn parse_iperf3_json_defaults_retransmits_when_missing() {
        let sample = r#"{
            "end": {
                "streams": [{ "sender": {} }],
                "sum_received": {
                    "seconds": 5.0,
                    "bits_per_second": 1.0
                }
            }
        }"#;
        let m = parse_iperf3_json(sample).expect("parse");
        assert_eq!(m["iperf3_retransmits_count"], 0.0);
    }

    /// Missing headline field is a hard error.
    #[test]
    fn parse_iperf3_json_errors_on_missing_bps() {
        let sample = r#"{ "end": { "sum_received": { "seconds": 5.0 } } }"#;
        assert!(parse_iperf3_json(sample).is_err());
    }
}

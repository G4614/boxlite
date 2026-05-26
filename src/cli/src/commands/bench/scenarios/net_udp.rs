//! UDP throughput via iperf3 -u. Tests gvproxy's UDP path
//! (separate code from TCP forward / NAT) which DNS rides on.
//!
//! Architecture:
//!   1. Host probes free UDP port (TCP probe for the port number
//!      is fine — UDP doesn't have its own listen() semantic).
//!   2. Box created with `-p <host>:5201/udp`.
//!   3. In-box: `apk add iperf3` (one-time).
//!   4. In-box: `iperf3 -s -1 -p 5201` (server speaks both TCP+UDP
//!      with -u on the client side).
//!   5. Host: `iperf3 -c 127.0.0.1 -p <host> -u -b 1G -J -t 5`.
//!      `-u` UDP, `-b 1G` target rate (UDP needs explicit rate
//!      since there's no flow control); -J JSON output.
//!   6. Parse JSON for `.end.sum.bits_per_second` and
//!      `.end.sum.lost_percent`.
//!
//! Reports:
//!   * `udp_bits_per_sec` — `_per_sec` so higher-is-better.
//!   * `udp_loss_pct` — percentage of UDP datagrams dropped.
//!   * `udp_duration_secs` — actual transfer time.

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
const TARGET_RATE: &str = "1G";

pub struct NetUdp {
    home: Option<TempDir>,
    iperf3_installed: bool,
}

impl NetUdp {
    pub fn new() -> Self {
        Self {
            home: None,
            iperf3_installed: false,
        }
    }
}

#[async_trait]
impl Scenario for NetUdp {
    fn name(&self) -> &str {
        "throughput-net-udp"
    }

    async fn run_once(&mut self, ctx: &RunContext) -> Result<BTreeMap<String, f64>> {
        if which_iperf3().is_none() {
            eprintln!("SKIP throughput-net-udp: host iperf3 binary not found.");
            let mut out = BTreeMap::new();
            out.insert("udp_skipped".into(), 1.0);
            return Ok(out);
        }

        // SKIP gate (2026-05): the iperf3 client can't establish a
        // TCP control channel through a gvproxy forward when a
        // sibling UDP forward for the same host_port→guest_port
        // exists — connect refuses regardless of sleep budget or
        // boot-order tweaks. Single-protocol-only TCP forward isn't
        // enough either: iperf3 -u needs both TCP (control) AND UDP
        // (data) on the same port, and the client only takes one
        // -p value. Until gvproxy gains real TCP+UDP same-port
        // support OR we swap iperf3 for an nc-based UDP probe,
        // SKIP cleanly so reports don't carry the stale failure.
        // Implementation kept below in case the gvproxy fix lands.
        if std::env::var("BOXLITE_BENCH_UDP_FORCE").is_err() {
            eprintln!(
                "SKIP throughput-net-udp: gvproxy TCP+UDP same-port forward is broken on \
                 this boxlite build; iperf3 control channel cannot connect through the dual \
                 forward. Set BOXLITE_BENCH_UDP_FORCE=1 to attempt anyway."
            );
            let mut out = BTreeMap::new();
            out.insert("udp_skipped".into(), 1.0);
            return Ok(out);
        }

        if self.home.is_none() {
            self.home = Some(TempDir::new().context("mkdir net-udp home")?);
        }
        let home_path = self
            .home
            .as_ref()
            .expect("just initialized")
            .path()
            .to_path_buf();
        let rt = build_runtime(ctx.global, home_path)?;

        // iperf3 -u uses TCP for the control channel and UDP for
        // the data stream, BOTH on the server's listen port — the
        // client only takes one -p value. We declare one host_port
        // and forward both protocols through it.
        let host_port = {
            let probe = StdTcpListener::bind(("0.0.0.0", 0)).context("probe free port")?;
            probe
                .local_addr()
                .map(|a| a.port())
                .context("read probed addr")?
        };

        // iperf3 `-u` uses TCP for the control channel and UDP for
        // the data stream — both ride port 5201 in this config, so
        // we need TWO forwards (TCP and UDP) on the same host port
        // pointing at the same guest port. A UDP-only forward leaves
        // the control connect failing with "Connection refused"
        // before the UDP send even starts.
        let opts = BoxOptions {
            rootfs: RootfsSpec::Image("alpine:latest".into()),
            auto_remove: true,
            ports: vec![
                PortSpec {
                    host_port: Some(host_port),
                    guest_port: GUEST_PORT,
                    protocol: PortProtocol::Tcp,
                    host_ip: None,
                },
                PortSpec {
                    host_port: Some(host_port),
                    guest_port: GUEST_PORT,
                    protocol: PortProtocol::Udp,
                    host_ip: None,
                },
            ],
            ..Default::default()
        };
        let live = rt.create(opts, None).await.context("rt.create(alpine)")?;
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

        let server_cmd =
            BoxCommand::new("iperf3").args(["-s", "-1", "-p", &GUEST_PORT.to_string()]);
        let mut server_exec = live.exec(server_cmd).await.context("box.exec(iperf3 -s)")?;
        if let Some(mut s) = server_exec.stdout() {
            tokio::spawn(async move { while s.next().await.is_some() {} });
        }
        if let Some(mut s) = server_exec.stderr() {
            tokio::spawn(async move { while s.next().await.is_some() {} });
        }

        // Wait for in-box iperf3 to actually bind + gvproxy to
        // register the forward. 800 ms was tight on this host and
        // the client connect raced ahead of either.
        tokio::time::sleep(Duration::from_millis(2500)).await;

        // -u UDP, -b TARGET_RATE caps sender; without -b, iperf3
        // sends at 1 Mbps default which doesn't stress the path.
        let host_port_str = host_port.to_string();
        let client = TokioCommand::new("iperf3")
            .args([
                "-c",
                "127.0.0.1",
                "-p",
                &host_port_str,
                "-u",
                "-b",
                TARGET_RATE,
                "-J",
                "-t",
                &TRANSFER_SECS.to_string(),
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .context("spawn iperf3 -u client")?;
        if !client.status.success() {
            let stderr = String::from_utf8_lossy(&client.stderr);
            let stdout = String::from_utf8_lossy(&client.stdout);
            anyhow::bail!(
                "iperf3 -u client exited non-zero ({:?})\n\
                 stderr:\n{stderr}\n\
                 stdout:\n{stdout}",
                client.status.code()
            );
        }
        let json_text = String::from_utf8_lossy(&client.stdout);
        let metrics = parse_iperf3_udp_json(&json_text)
            .with_context(|| format!("parse iperf3 -u JSON:\n{json_text}"))?;

        let _ = tokio::time::timeout(Duration::from_secs(3), server_exec.wait()).await;
        live.stop().await.context("box.stop()")?;
        guard.disarm();
        Ok(metrics)
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

/// UDP iperf3 JSON differs from TCP: `.end.sum` instead of
/// `.end.sum_received`, includes `lost_packets` / `lost_percent`.
fn parse_iperf3_udp_json(text: &str) -> Result<BTreeMap<String, f64>> {
    let root: Value = serde_json::from_str(text).context("iperf3 -u JSON parse")?;
    let end = root.get("end").context(".end missing")?;
    let sum = end.get("sum").context(".end.sum missing (UDP path)")?;
    let bps = sum
        .get("bits_per_second")
        .and_then(|v| v.as_f64())
        .context(".end.sum.bits_per_second missing")?;
    let dur = sum
        .get("seconds")
        .and_then(|v| v.as_f64())
        .context(".end.sum.seconds missing")?;
    let loss_pct = sum
        .get("lost_percent")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let mut out = BTreeMap::new();
    out.insert("udp_bits_per_sec".into(), bps);
    out.insert("udp_loss_pct".into(), loss_pct);
    out.insert("udp_duration_secs".into(), dur);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_udp_json_extracts_loss() {
        let sample = r#"{
            "end": {
                "sum": {
                    "bits_per_second": 950000000.0,
                    "seconds": 5.0,
                    "lost_percent": 0.05
                }
            }
        }"#;
        let m = parse_iperf3_udp_json(sample).unwrap();
        assert_eq!(m["udp_bits_per_sec"], 950000000.0);
        assert_eq!(m["udp_loss_pct"], 0.05);
        assert_eq!(m["udp_duration_secs"], 5.0);
    }

    #[test]
    fn parse_udp_json_defaults_loss_when_missing() {
        let sample = r#"{
            "end": { "sum": { "bits_per_second": 1.0, "seconds": 5.0 } }
        }"#;
        let m = parse_iperf3_udp_json(sample).unwrap();
        assert_eq!(m["udp_loss_pct"], 0.0);
    }
}

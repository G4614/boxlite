//! Multi-stream iperf3 — measures gvproxy's parallel-connection
//! handling. Single-stream `throughput-net-iperf3` is bottlenecked
//! by single-core gvproxy hot-path performance; running `-P 4`
//! exercises the netstack's concurrent forward path and exposes
//! per-flow fairness / aggregate scaling.
//!
//! Same setup as `throughput-net-iperf3` (host client, in-box
//! server) except `iperf3 -P 4` on the client. The server handles
//! 4 streams automatically. JSON output includes a per-stream
//! breakdown; we report the aggregate `sum_received` headline +
//! the per-stream stdev so a scaling regression (e.g., one stream
//! starves the others) shows up.

use super::super::runner::{RunContext, Scenario};
use super::common::{BoxGuard, build_runtime};
use anyhow::{Context, Result};
use async_trait::async_trait;
use boxlite::runtime::options::{PortProtocol, PortSpec, RootfsSpec};
use boxlite::{BoxCommand, BoxOptions};
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
const STREAMS: u32 = 4;

pub struct NetIperf3Parallel {
    home: Option<TempDir>,
    iperf3_installed: bool,
}

impl NetIperf3Parallel {
    pub fn new() -> Self {
        Self {
            home: None,
            iperf3_installed: false,
        }
    }
}

#[async_trait]
impl Scenario for NetIperf3Parallel {
    fn name(&self) -> &str {
        "throughput-net-iperf3-parallel"
    }

    async fn run_once(&mut self, ctx: &RunContext) -> Result<BTreeMap<String, f64>> {
        if which_iperf3().is_none() {
            eprintln!("SKIP throughput-net-iperf3-parallel: host iperf3 missing.");
            let mut out = BTreeMap::new();
            out.insert("iperf3_parallel_skipped".into(), 1.0);
            return Ok(out);
        }

        if self.home.is_none() {
            self.home = Some(TempDir::new().context("mkdir net-parallel home")?);
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

        tokio::time::sleep(Duration::from_millis(800)).await;

        let client = TokioCommand::new("iperf3")
            .args([
                "-c",
                "127.0.0.1",
                "-p",
                &host_port.to_string(),
                "-P",
                &STREAMS.to_string(),
                "-J",
                "-t",
                &TRANSFER_SECS.to_string(),
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .context("spawn iperf3 -P client")?;
        if !client.status.success() {
            let stderr = String::from_utf8_lossy(&client.stderr);
            anyhow::bail!(
                "iperf3 -P client exited non-zero ({:?}); stderr:\n{stderr}",
                client.status.code()
            );
        }
        let json_text = String::from_utf8_lossy(&client.stdout);
        let metrics = parse_iperf3_parallel_json(&json_text)
            .with_context(|| format!("parse iperf3 -P JSON:\n{json_text}"))?;

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

/// `-P N` adds `.end.streams[]` with N entries. We aggregate:
///   * sum of bits_per_second across streams (headline)
///   * per-stream bits_per_second → stdev (fairness)
///   * retransmits total
fn parse_iperf3_parallel_json(text: &str) -> Result<BTreeMap<String, f64>> {
    let root: Value = serde_json::from_str(text).context("iperf3 JSON parse")?;
    let end = root.get("end").context(".end missing")?;
    let sum_recv = end
        .get("sum_received")
        .context(".end.sum_received missing")?;
    let bps_total = sum_recv
        .get("bits_per_second")
        .and_then(|v| v.as_f64())
        .context(".end.sum_received.bits_per_second missing")?;
    let dur = sum_recv
        .get("seconds")
        .and_then(|v| v.as_f64())
        .context(".end.sum_received.seconds missing")?;

    let streams = end
        .get("streams")
        .and_then(|s| s.as_array())
        .context(".end.streams missing")?;
    let per_stream: Vec<f64> = streams
        .iter()
        .filter_map(|s| s.get("receiver"))
        .filter_map(|r| r.get("bits_per_second").and_then(|v| v.as_f64()))
        .collect();
    let retrans_total: f64 = streams
        .iter()
        .filter_map(|s| s.get("sender"))
        .filter_map(|s| s.get("retransmits").and_then(|v| v.as_f64()))
        .sum();
    let (stream_mean, stream_stdev) = if per_stream.len() > 1 {
        let mean = per_stream.iter().sum::<f64>() / per_stream.len() as f64;
        let var = per_stream.iter().map(|x| (x - mean).powi(2)).sum::<f64>()
            / (per_stream.len() as f64 - 1.0);
        (mean, var.sqrt())
    } else {
        (per_stream.first().copied().unwrap_or(0.0), 0.0)
    };

    let mut out = BTreeMap::new();
    out.insert("parallel_iperf3_bits_per_sec".into(), bps_total);
    out.insert("parallel_iperf3_per_stream_mean_bps".into(), stream_mean);
    out.insert("parallel_iperf3_per_stream_stdev_bps".into(), stream_stdev);
    out.insert("parallel_iperf3_retransmits_count".into(), retrans_total);
    out.insert(
        "parallel_iperf3_streams_count".into(),
        per_stream.len() as f64,
    );
    out.insert("parallel_iperf3_duration_secs".into(), dur);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_parallel_json_aggregates_streams() {
        // Two streams: 400 Mbps + 600 Mbps. Mean = 500, stdev = 141.42.
        let sample = r#"{
            "end": {
                "sum_received": {
                    "bits_per_second": 1000000000.0,
                    "seconds": 5.0
                },
                "streams": [
                    { "receiver": { "bits_per_second": 400000000.0 },
                      "sender": { "retransmits": 1 } },
                    { "receiver": { "bits_per_second": 600000000.0 },
                      "sender": { "retransmits": 2 } }
                ]
            }
        }"#;
        let m = parse_iperf3_parallel_json(sample).unwrap();
        assert_eq!(m["parallel_iperf3_bits_per_sec"], 1_000_000_000.0);
        assert_eq!(m["parallel_iperf3_per_stream_mean_bps"], 500_000_000.0);
        assert!((m["parallel_iperf3_per_stream_stdev_bps"] - 141_421_356.24).abs() < 1.0);
        assert_eq!(m["parallel_iperf3_retransmits_count"], 3.0);
        assert_eq!(m["parallel_iperf3_streams_count"], 2.0);
    }
}

//! In-box `fio` disk benchmark — IOPS + latency percentiles.
//!
//! Complements `throughput-disk-write` (which is dd-based and only
//! reports sequential-write MB/s). fio adds:
//!
//!   * Random-write IOPS (4K) — the metric most users actually care
//!     about for DB / log-heavy workloads.
//!   * Latency percentiles (clat p50/p99/p999) — tail behavior dd
//!     cannot report.
//!   * Sync-write coverage (`--fsync=1`) — exercises the qcow2
//!     overlay's commit path.
//!
//! `fio` is not in the alpine base, so the scenario does a one-time
//! `apk add fio` inside the box's first iteration via a separate exec.
//! Subsequent iterations reuse the same `--home` (warm cache, fio
//! still installed in the COW overlay) so the install cost is paid
//! once.
//!
//! Reports (all from fio's `--output-format=json` stdout, parsed via
//! serde_json):
//!   * `disk_fio_iops`            — writes per second (4K random).
//!   * `disk_fio_bw_kb_per_sec`   — fio's bw_mean for the write job
//!     (kB/s; metric name suffix `_per_sec` flips `higher_is_better`).
//!   * `disk_fio_clat_p50_ns`     — completion-latency p50, nanoseconds.
//!   * `disk_fio_clat_p99_ns`     — completion-latency p99.
//!   * `disk_fio_clat_p999_ns`    — completion-latency p99.9 (tail).
//!
//! All clat metrics use `_ns` suffix; the report's unit hint resolves
//! to "?" but the metric name carries the unit unambiguously.

use super::super::runner::{RunContext, Scenario};
use super::common::{BoxGuard, alpine_options, build_runtime};
use anyhow::{Context, Result};
use async_trait::async_trait;
use boxlite::BoxCommand;
use futures::StreamExt;
use serde_json::Value;
use std::collections::BTreeMap;
use tempfile::TempDir;

/// 64 MiB working set — same as `throughput-disk-write` so the two
/// scenarios are directly comparable (sequential vs random pattern
/// over the same payload size).
const SIZE_BYTES: &str = "64M";
/// Wall time the fio job runs for. The 30 s default gives the
/// random-write workload time to settle past the COW first-touch
/// burst.
const RUNTIME_SECS: &str = "10";
const BLOCK_SIZE: &str = "4K";

pub struct DiskFio {
    home: Option<TempDir>,
    fio_installed: bool,
}

impl DiskFio {
    pub fn new() -> Self {
        Self {
            home: None,
            fio_installed: false,
        }
    }
}

#[async_trait]
impl Scenario for DiskFio {
    fn name(&self) -> &str {
        "throughput-disk-fio"
    }

    async fn run_once(&mut self, ctx: &RunContext) -> Result<BTreeMap<String, f64>> {
        if self.home.is_none() {
            self.home = Some(TempDir::new().context("mkdir disk-fio home")?);
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

        // One-shot fio install on the first iteration. The COW
        // overlay carries the install across subsequent iterations
        // (shared `--home`), so this is amortized.
        if !self.fio_installed {
            let install = BoxCommand::new("apk").args(["add", "--no-cache", "fio"]);
            let mut exec = live.exec(install).await.context("apk add fio")?;
            // Drain stdout/stderr while we wait so the channel
            // buffers don't get stale.
            if let Some(mut s) = exec.stdout() {
                tokio::spawn(async move { while s.next().await.is_some() {} });
            }
            if let Some(mut s) = exec.stderr() {
                tokio::spawn(async move { while s.next().await.is_some() {} });
            }
            let r = exec.wait().await.context("apk add fio wait")?;
            if r.exit_code != 0 {
                anyhow::bail!("apk add fio failed (exit {})", r.exit_code);
            }
            self.fio_installed = true;
        }

        // The fio job: 4K random writes with fsync=1 (every write
        // committed). JSON output for unambiguous parsing.
        let fio_cmd = BoxCommand::new("fio").args([
            "--name=randwrite",
            "--rw=randwrite",
            &format!("--bs={}", BLOCK_SIZE),
            &format!("--size={}", SIZE_BYTES),
            &format!("--runtime={}", RUNTIME_SECS),
            "--time_based",
            "--ioengine=sync",
            "--fsync=1",
            "--directory=/tmp",
            "--output-format=json",
        ]);
        let mut exec = live.exec(fio_cmd).await.context("box.exec(fio)")?;

        // fio writes its JSON report to stdout. Drain stderr in
        // parallel and CAPTURE it so a non-zero exit can surface
        // the actual reason instead of an empty stdout.
        let mut stdout = exec.stdout().expect("stdout handle present");
        let mut stderr = exec.stderr().expect("stderr handle present");
        let stderr_buf = std::sync::Arc::new(tokio::sync::Mutex::new(String::new()));
        let stderr_writer = std::sync::Arc::clone(&stderr_buf);
        let stderr_drain = tokio::spawn(async move {
            while let Some(chunk) = stderr.next().await {
                stderr_writer.lock().await.push_str(&chunk);
            }
        });
        let mut stdout_text = String::new();
        while let Some(chunk) = stdout.next().await {
            stdout_text.push_str(&chunk);
        }
        let _ = stderr_drain.await;
        let stderr_text = stderr_buf.lock().await.clone();

        let r = exec.wait().await.context("fio exec wait")?;
        if r.exit_code != 0 {
            anyhow::bail!(
                "fio exited non-zero ({}); stderr:\n{stderr_text}\nstdout:\n{stdout_text}",
                r.exit_code,
            );
        }

        let metrics = parse_fio_json(&stdout_text)
            .with_context(|| format!("parse fio JSON output:\n{stdout_text}"))?;

        live.stop().await.context("box.stop()")?;
        guard.disarm();
        Ok(metrics)
    }
}

/// Extract our headline metrics from fio's JSON output. fio's schema
/// is documented at <https://fio.readthedocs.io/en/latest/fio_doc.html>
/// — the relevant path for a single-job run is:
///
///   `.jobs[0].write.iops`
///   `.jobs[0].write.bw`               (kB/s, mean)
///   `.jobs[0].write.clat_ns.percentile."50.000000"`  (and "99.000000", "99.900000")
///
/// Failing to find any of those returns an `Err` so a future fio
/// version that renames a field surfaces loudly instead of reporting
/// `0.0` and looking like a regression.
fn parse_fio_json(text: &str) -> Result<BTreeMap<String, f64>> {
    let root: Value = serde_json::from_str(text).context("fio JSON parse")?;
    let job = root
        .get("jobs")
        .and_then(|j| j.as_array())
        .and_then(|a| a.first())
        .context("jobs[0] missing in fio output")?;
    let write = job
        .get("write")
        .context("jobs[0].write missing in fio output")?;

    let iops = write
        .get("iops")
        .and_then(|v| v.as_f64())
        .context("jobs[0].write.iops missing")?;
    let bw_kb = write
        .get("bw")
        .and_then(|v| v.as_f64())
        .context("jobs[0].write.bw missing")?;
    let clat = write
        .get("clat_ns")
        .and_then(|v| v.get("percentile"))
        .context("jobs[0].write.clat_ns.percentile missing")?;
    let p50 = clat
        .get("50.000000")
        .and_then(|v| v.as_f64())
        .context("clat p50 missing")?;
    let p99 = clat
        .get("99.000000")
        .and_then(|v| v.as_f64())
        .context("clat p99 missing")?;
    let p999 = clat
        .get("99.900000")
        .and_then(|v| v.as_f64())
        .context("clat p99.9 missing")?;

    let mut out = BTreeMap::new();
    // `iops` doesn't have a suffix the unit_hint table recognizes;
    // we rename to `..._per_sec` so the comparator's direction
    // logic flips correctly (higher IOPS = better).
    out.insert("disk_fio_iops_per_sec".into(), iops);
    out.insert("disk_fio_bw_kb_per_sec".into(), bw_kb);
    out.insert("disk_fio_clat_p50_ns".into(), p50);
    out.insert("disk_fio_clat_p99_ns".into(), p99);
    out.insert("disk_fio_clat_p999_ns".into(), p999);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pin the fio JSON parser against a stripped-down sample that
    /// matches the real schema. If fio renames a field in a future
    /// version this test fails loudly rather than the scenario
    /// silently reporting `0.0`.
    #[test]
    fn parse_fio_json_extracts_headline_metrics() {
        let sample = r#"{
            "jobs": [{
                "write": {
                    "iops": 12345.6,
                    "bw": 49382,
                    "clat_ns": {
                        "percentile": {
                            "50.000000": 8100,
                            "99.000000": 42000,
                            "99.900000": 180000
                        }
                    }
                }
            }]
        }"#;
        let m = parse_fio_json(sample).expect("parse");
        assert_eq!(m["disk_fio_iops_per_sec"], 12345.6);
        assert_eq!(m["disk_fio_bw_kb_per_sec"], 49382.0);
        assert_eq!(m["disk_fio_clat_p50_ns"], 8100.0);
        assert_eq!(m["disk_fio_clat_p99_ns"], 42000.0);
        assert_eq!(m["disk_fio_clat_p999_ns"], 180000.0);
    }

    /// Missing field → `Err`, not a silent `0.0`. The scenario
    /// surfaces fio's raw output in the error context so the
    /// renamed field is easy to spot.
    #[test]
    fn parse_fio_json_errors_on_missing_field() {
        // `iops` deleted.
        let sample = r#"{
            "jobs": [{
                "write": {
                    "bw": 49382,
                    "clat_ns": { "percentile": {
                        "50.000000": 0, "99.000000": 0, "99.900000": 0
                    } }
                }
            }]
        }"#;
        assert!(parse_fio_json(sample).is_err());
    }
}

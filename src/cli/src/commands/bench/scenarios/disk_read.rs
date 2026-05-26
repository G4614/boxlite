//! In-box disk READ throughput — counterpart to `throughput-disk-
//! write` and `throughput-disk-fio` (both write-side).
//!
//! Two scenarios share this file:
//!
//!   * `throughput-disk-read` — `dd if=/tmp/big of=/dev/null bs=1M`,
//!     parses dd's summary line. Sequential read MB/s. Reads through
//!     the same qcow2-COW-over-virtio path the write scenarios
//!     test, so the read-vs-write delta is a useful aggregate
//!     signal (if writes regressed but reads didn't, the cost is
//!     in the COW commit path; if both regressed equally, it's
//!     virtio-blk).
//!
//!   * `throughput-disk-fio-read` — `fio` 4K random reads,
//!     completion-latency percentiles. Same shape as
//!     `throughput-disk-fio` but `--rw=randread`.
//!
//! Both pre-stage a 64 MiB working file via `dd if=/dev/zero ...`
//! before the read measurement so the read isn't just a sparse-
//! file zero-page hit.

use super::super::runner::{RunContext, Scenario};
use super::common::{BoxGuard, alpine_options, build_runtime};
use anyhow::{Context, Result};
use async_trait::async_trait;
use boxlite::{BoxCommand, LiteBox};
use futures::StreamExt;
use serde_json::Value;
use std::collections::BTreeMap;
use std::time::Instant;
use tempfile::TempDir;

const WORKING_SET_MB: u32 = 64;
const FIO_RUNTIME_SECS: u32 = 10;

/// Stage `/tmp/bench-read-src` as a 64 MiB dense file so the read
/// loop doesn't get sparse-file zero-page short-circuited. Idempotent:
/// re-running the stage on subsequent iterations just overwrites
/// the same file with the same content.
async fn stage_read_file(live: &LiteBox) -> Result<()> {
    let cmd = BoxCommand::new("dd").args([
        "if=/dev/zero",
        "of=/tmp/bench-read-src",
        "bs=1M",
        &format!("count={WORKING_SET_MB}"),
        "conv=fsync",
    ]);
    let mut exec = live.exec(cmd).await.context("box.exec(dd stage)")?;
    if let Some(mut s) = exec.stdout() {
        tokio::spawn(async move { while s.next().await.is_some() {} });
    }
    if let Some(mut s) = exec.stderr() {
        tokio::spawn(async move { while s.next().await.is_some() {} });
    }
    let r = exec.wait().await.context("dd stage wait")?;
    if r.exit_code != 0 {
        anyhow::bail!("dd stage failed (exit {})", r.exit_code);
    }
    Ok(())
}

/// Drop the page cache before each read measurement so we test
/// the actual disk path, not the page-cache hit rate. `echo 3 >
/// /proc/sys/vm/drop_caches` requires CAP_SYS_ADMIN; boxlite's
/// default non-privileged container can't write it, so we instead
/// recreate the file (which invalidates any cache of the previous
/// inode). For the `bench-read-src` size class that's a few
/// hundred ms — acceptable.
async fn invalidate_read_cache(live: &LiteBox) -> Result<()> {
    stage_read_file(live).await
}

/// Parse `dd`'s stderr summary line for MB/s — same parser as
/// `throughput-disk-write` would use; duplicated here to keep the
/// scenarios independent of each other.
fn parse_dd_mb_per_sec(text: &str) -> Result<f64> {
    for line in text.lines().rev() {
        let (unit_pos, multiplier) = if let Some(pos) = line.rfind("MB/s") {
            (pos, 1.0)
        } else if let Some(pos) = line.rfind("GB/s") {
            (pos, 1024.0)
        } else if let Some(pos) = line.rfind("kB/s") {
            (pos, 1.0 / 1024.0)
        } else {
            continue;
        };
        let head = &line[..unit_pos].trim_end();
        let tok = head
            .split(|c: char| c.is_whitespace() || c == ',')
            .next_back()
            .unwrap_or("");
        if let Ok(v) = tok.parse::<f64>() {
            return Ok(v * multiplier);
        }
    }
    anyhow::bail!("no parseable rate line in dd output")
}

// ─── throughput-disk-read ─────────────────────────────────────────

pub struct DiskRead {
    home: Option<TempDir>,
    file_staged: bool,
}

impl DiskRead {
    pub fn new() -> Self {
        Self {
            home: None,
            file_staged: false,
        }
    }
}

#[async_trait]
impl Scenario for DiskRead {
    fn name(&self) -> &str {
        "throughput-disk-read"
    }

    async fn run_once(&mut self, ctx: &RunContext) -> Result<BTreeMap<String, f64>> {
        if self.home.is_none() {
            self.home = Some(TempDir::new().context("mkdir disk-read home")?);
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

        if !self.file_staged {
            stage_read_file(&live).await?;
            self.file_staged = true;
        } else {
            invalidate_read_cache(&live).await?;
        }

        let read_cmd =
            BoxCommand::new("dd").args(["if=/tmp/bench-read-src", "of=/dev/null", "bs=1M"]);
        let exec_start = Instant::now();
        let mut exec = live.exec(read_cmd).await.context("box.exec(dd read)")?;
        let mut stderr = exec.stderr().expect("stderr handle");
        let mut stderr_text = String::new();
        while let Some(chunk) = stderr.next().await {
            stderr_text.push_str(&chunk);
        }
        if let Some(mut s) = exec.stdout() {
            tokio::spawn(async move { while s.next().await.is_some() {} });
        }
        let r = exec.wait().await.context("dd read wait")?;
        if r.exit_code != 0 {
            anyhow::bail!(
                "dd read exited non-zero ({}); stderr:\n{stderr_text}",
                r.exit_code
            );
        }
        let wall_ms = exec_start.elapsed().as_secs_f64() * 1000.0;
        let mb_per_sec = parse_dd_mb_per_sec(&stderr_text)
            .with_context(|| format!("parse dd output:\n{stderr_text}"))?;

        live.stop().await.context("box.stop()")?;
        guard.disarm();

        let mut metrics = BTreeMap::new();
        metrics.insert("disk_read_mb_per_sec".into(), mb_per_sec);
        metrics.insert(
            "disk_read_bytes".into(),
            (WORKING_SET_MB as u64 * 1024 * 1024) as f64,
        );
        metrics.insert("disk_read_wall_ms".into(), wall_ms);
        Ok(metrics)
    }
}

// ─── throughput-disk-fio-read ─────────────────────────────────────

pub struct DiskFioRead {
    home: Option<TempDir>,
    fio_installed: bool,
    file_staged: bool,
}

impl DiskFioRead {
    pub fn new() -> Self {
        Self {
            home: None,
            fio_installed: false,
            file_staged: false,
        }
    }
}

#[async_trait]
impl Scenario for DiskFioRead {
    fn name(&self) -> &str {
        "throughput-disk-fio-read"
    }

    async fn run_once(&mut self, ctx: &RunContext) -> Result<BTreeMap<String, f64>> {
        if self.home.is_none() {
            self.home = Some(TempDir::new().context("mkdir disk-fio-read home")?);
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

        if !self.fio_installed {
            let install = BoxCommand::new("apk").args(["add", "--no-cache", "fio"]);
            let mut exec = live.exec(install).await.context("apk add fio")?;
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

        if !self.file_staged {
            stage_read_file(&live).await?;
            self.file_staged = true;
        }

        let fio_cmd = BoxCommand::new("fio").args([
            "--name=randread",
            "--rw=randread",
            "--bs=4K",
            &format!("--size={WORKING_SET_MB}M"),
            &format!("--runtime={FIO_RUNTIME_SECS}"),
            "--time_based",
            "--ioengine=sync",
            "--filename=/tmp/bench-read-src",
            "--output-format=json",
        ]);
        let mut exec = live.exec(fio_cmd).await.context("box.exec(fio randread)")?;
        let mut stdout = exec.stdout().expect("stdout handle");
        let mut stdout_text = String::new();
        while let Some(chunk) = stdout.next().await {
            stdout_text.push_str(&chunk);
        }
        if let Some(mut s) = exec.stderr() {
            tokio::spawn(async move { while s.next().await.is_some() {} });
        }
        let r = exec.wait().await.context("fio randread wait")?;
        if r.exit_code != 0 {
            anyhow::bail!(
                "fio randread exited non-zero ({}); stdout:\n{stdout_text}",
                r.exit_code
            );
        }

        let metrics = parse_fio_read_json(&stdout_text)
            .with_context(|| format!("parse fio JSON:\n{stdout_text}"))?;

        live.stop().await.context("box.stop()")?;
        guard.disarm();
        Ok(metrics)
    }
}

fn parse_fio_read_json(text: &str) -> Result<BTreeMap<String, f64>> {
    let root: Value = serde_json::from_str(text).context("fio JSON parse")?;
    let job = root
        .get("jobs")
        .and_then(|j| j.as_array())
        .and_then(|a| a.first())
        .context("jobs[0] missing")?;
    let read = job.get("read").context("jobs[0].read missing")?;
    let iops = read
        .get("iops")
        .and_then(|v| v.as_f64())
        .context("jobs[0].read.iops missing")?;
    let bw_kb = read
        .get("bw")
        .and_then(|v| v.as_f64())
        .context("jobs[0].read.bw missing")?;
    let clat = read
        .get("clat_ns")
        .and_then(|v| v.get("percentile"))
        .context("jobs[0].read.clat_ns.percentile missing")?;
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
    out.insert("disk_read_fio_iops_per_sec".into(), iops);
    out.insert("disk_read_fio_bw_kb_per_sec".into(), bw_kb);
    out.insert("disk_read_fio_clat_p50_ns".into(), p50);
    out.insert("disk_read_fio_clat_p99_ns".into(), p99);
    out.insert("disk_read_fio_clat_p999_ns".into(), p999);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_fio_read_json_extracts_headlines() {
        let sample = r#"{
            "jobs": [{
                "read": {
                    "iops": 50000.5,
                    "bw": 200000,
                    "clat_ns": {
                        "percentile": {
                            "50.000000": 10000,
                            "99.000000": 50000,
                            "99.900000": 200000
                        }
                    }
                }
            }]
        }"#;
        let m = parse_fio_read_json(sample).unwrap();
        assert_eq!(m["disk_read_fio_iops_per_sec"], 50000.5);
        assert_eq!(m["disk_read_fio_bw_kb_per_sec"], 200000.0);
        assert_eq!(m["disk_read_fio_clat_p99_ns"], 50000.0);
    }

    #[test]
    fn parse_dd_mb_per_sec_handles_busybox_and_gnu() {
        assert_eq!(
            parse_dd_mb_per_sec("67108864 bytes (67 MB, 64 MiB) copied, 0.05 s, 1300 MB/s\n")
                .unwrap(),
            1300.0
        );
        assert_eq!(
            parse_dd_mb_per_sec("67108864 bytes (64.0MB) copied, 0.05 seconds, 1300.0MB/s\n")
                .unwrap(),
            1300.0
        );
    }
}

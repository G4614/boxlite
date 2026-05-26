//! In-box disk throughput scenario.
//!
//! `throughput-disk-write` — measure sequential write speed of the
//! container's COW overlay by running `dd if=/dev/zero of=/tmp/bench
//! bs=1M count=64 conv=fsync` inside the box and parsing dd's own
//! "MB/s" line. Why this approach over fio:
//!
//!   * Zero workload-image cost — busybox `dd` ships with alpine,
//!     so we don't need a custom test image with fio vendored in.
//!   * `conv=fsync` forces dd to flush before reporting, so the
//!     number reflects real durable-write throughput (not just
//!     page-cache hit rate).
//!   * `/tmp` is tmpfs on alpine by default — but inside boxlite's
//!     guest, `/tmp` is on the COW overlay (the container rootfs
//!     virtio-blk device). So this DOES measure qcow2-COW-over-
//!     virtio bandwidth, which is the headline number for any
//!     boxlite disk-stack regression.
//!
//! Reported metrics:
//!   * `disk_write_mb_per_sec` — parsed from dd's summary line.
//!   * `disk_write_bytes` — bytes the dd call asked to write (a
//!     const, captured to avoid cross-version comparisons of MB/s
//!     that secretly used different working-set sizes).
//!   * `disk_write_wall_ms` — wall-clocked from the host's view of
//!     the exec, lower-bounded by the in-box dd time. Includes the
//!     exec round-trip cost; for the pure in-box write rate use
//!     `disk_write_mb_per_sec`.
//!
//! Shared `--home` across iterations (warm cache) — first iteration
//! pays the alpine pull + base disk build, subsequent ones measure
//! the actual write path.

use super::super::runner::{RunContext, Scenario};
use super::common::{BoxGuard, alpine_options, build_runtime};
use anyhow::{Context, Result};
use async_trait::async_trait;
use boxlite::BoxCommand;
use futures::StreamExt;
use std::collections::BTreeMap;
use std::time::Instant;
use tempfile::TempDir;

/// 64 MiB write. Big enough to be larger than typical page-cache
/// hot-blocks for a fresh alpine box, small enough to keep an
/// iteration under ~10 s on cheap hardware.
const WRITE_BYTES: u64 = 64 * 1024 * 1024;
const WRITE_COUNT: u64 = 64;
const WRITE_BS: &str = "1M";

pub struct DiskWrite {
    home: Option<TempDir>,
}

impl DiskWrite {
    pub fn new() -> Self {
        Self { home: None }
    }
}

#[async_trait]
impl Scenario for DiskWrite {
    fn name(&self) -> &str {
        "throughput-disk-write"
    }

    async fn run_once(&mut self, ctx: &RunContext) -> Result<BTreeMap<String, f64>> {
        if self.home.is_none() {
            self.home = Some(TempDir::new().context("mkdir disk-write home")?);
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

        // Build the dd command. `conv=fsync` is essential — without
        // it, dd returns when the page cache accepts the write,
        // which inflates the rate by ~100x on a tmpfs-backed
        // overlay.
        let cmd = BoxCommand::new("dd").args([
            "if=/dev/zero",
            "of=/tmp/bench-disk-write",
            &format!("bs={}", WRITE_BS),
            &format!("count={}", WRITE_COUNT),
            "conv=fsync",
        ]);
        let exec_start = Instant::now();
        let mut exec = live.exec(cmd).await.context("box.exec(dd)")?;

        // dd writes its summary to stderr. Drain stdout in parallel
        // so it doesn't fill the channel buffer and stall dd.
        let mut stderr = exec.stderr().expect("stderr handle should be present");
        let mut stdout = exec.stdout().expect("stdout handle should be present");
        let stdout_pump = tokio::spawn(async move { while stdout.next().await.is_some() {} });
        let mut stderr_text = String::new();
        while let Some(chunk) = stderr.next().await {
            stderr_text.push_str(&chunk);
        }
        let _ = stdout_pump.await;

        let result = exec.wait().await.context("dd exec wait")?;
        if result.exit_code != 0 {
            anyhow::bail!(
                "dd exited non-zero ({:?}); stderr:\n{}",
                result.exit_code,
                stderr_text
            );
        }
        let wall_ms = exec_start.elapsed().as_secs_f64() * 1000.0;

        let mb_per_sec = parse_dd_mb_per_sec(&stderr_text)
            .with_context(|| format!("parse dd output; raw stderr was:\n{stderr_text}"))?;

        live.stop().await.context("box.stop()")?;
        guard.disarm();

        let mut metrics = BTreeMap::new();
        metrics.insert("disk_write_mb_per_sec".into(), mb_per_sec);
        metrics.insert("disk_write_bytes".into(), WRITE_BYTES as f64);
        metrics.insert("disk_write_wall_ms".into(), wall_ms);
        Ok(metrics)
    }
}

/// Parse the MB/s rate from dd's terminal summary line.
///
/// GNU dd format:
///   `67108864 bytes (67 MB, 64 MiB) copied, 0.123 s, 545 MB/s`
/// busybox dd format:
///   `67108864 bytes (64.0MB) copied, 0.123 seconds, 540.0 MB/s`
///   (sometimes also `... 540.0MB/s` — no space before the unit)
///
/// We tokenize the last line containing "MB/s" or "GB/s", walk back
/// to the numeric token, and return its value (after normalizing
/// "GB/s" to MB by multiplying by 1024). A failed parse returns
/// `Err` so the scenario can surface dd's raw output to the user
/// instead of silently reporting 0.
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
        // busybox: "540.0MB/s" (no space). GNU: "540 MB/s" (space).
        // Split on whitespace, find the LAST token whose stripped
        // form parses as a float and which terminates either with
        // the unit or with whitespace before the unit.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// GNU dd's space-separated summary line.
    #[test]
    fn parse_gnu_dd_mb_per_sec() {
        let out = "67108864 bytes (67 MB, 64 MiB) copied, 0.123 s, 545 MB/s\n";
        assert_eq!(parse_dd_mb_per_sec(out).unwrap(), 545.0);
    }

    /// busybox dd squishes the unit against the number.
    #[test]
    fn parse_busybox_dd_mb_per_sec() {
        let out = "67108864 bytes (64.0MB) copied, 0.123 seconds, 540.0MB/s\n";
        let v = parse_dd_mb_per_sec(out).unwrap();
        assert!((v - 540.0).abs() < 1e-9, "got {v}, expected 540.0");
    }

    /// GB/s normalizes to 1024 × MB/s. Catches a regression where
    /// a fast NVMe disk would silently report 1 MB/s instead of
    /// 1024 MB/s.
    #[test]
    fn parse_gb_per_sec_normalizes_to_mb() {
        let out = "67108864 bytes copied, 0.001 s, 2.5 GB/s\n";
        let v = parse_dd_mb_per_sec(out).unwrap();
        assert!(
            (v - 2.5 * 1024.0).abs() < 1e-9,
            "got {v}, expected {}",
            2.5 * 1024.0
        );
    }

    /// Empty / garbage input must error, not silently return 0.
    #[test]
    fn parse_unparseable_errors() {
        assert!(parse_dd_mb_per_sec("").is_err());
        assert!(parse_dd_mb_per_sec("no rate here").is_err());
    }
}

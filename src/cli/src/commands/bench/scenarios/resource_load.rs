//! Box-under-load resource scenarios — complements `resource-idle`.
//!
//! `resource-idle` answers "what does an idle box cost". The two
//! scenarios here answer "what does it cost when actually working":
//!
//!   * `resource-cpu-load` — peg one vCPU at 100% with `stress-ng
//!     --cpu 1 --timeout 10s`, sample `BoxMetrics` every 2 s during.
//!     Reports mean+max CPU% under load and peak RSS. A regression
//!     where the libkrun shim grows RSS while the guest is CPU-busy
//!     shows up as `rss_load_max_bytes` increasing without any
//!     working-set change.
//!   * `resource-mem-pressure` — box capped at `MEM_LIMIT_MIB` MiB,
//!     allocate slightly less (`stress-ng --vm-bytes
//!     ALLOC_MIB --vm-keep`) and observe the ceiling. The headline
//!     is `rss_pressure_max_bytes` — peak RSS observed on the libkrun
//!     process while the guest was holding the allocation. A future
//!     change in the alloc / kernel / agent that pushes peak past
//!     the cgroup ceiling (`mem_pressure_limit_bytes`) shows up as a
//!     non-zero `mem_pressure_exit_code` — alpine's stress-ng exits
//!     1 even on the clean case (the "0=clean" semantic does not
//!     hold on this stress-ng build), so the actionable signal is
//!     a transition from `1` to `137` (SIGKILL by OOM-killer).
//!
//! Both share `--home` across iterations so `apk add --no-cache
//! stress-ng` is amortized. Default sample cadence is 2 s; both
//! workloads run for at least 8 s so each scenario collects ~4
//! samples before the workload exits.

use super::super::runner::{RunContext, Scenario};
use super::common::{BoxGuard, alpine_options, build_runtime};
use anyhow::{Context, Result};
use async_trait::async_trait;
use boxlite::runtime::options::RootfsSpec;
use boxlite::{BoxCommand, BoxOptions, LiteBox};
use futures::StreamExt;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};
use tempfile::TempDir;

const SAMPLE_INTERVAL: Duration = Duration::from_secs(2);
const CPU_LOAD_SECS: u64 = 10;
const MEM_PRESSURE_SECS: u64 = 8;
/// Memory limit on the box for `resource-mem-pressure`. Kept tight
/// (256 MiB) so the alloc + headroom math is unambiguous.
const MEM_LIMIT_MIB: u32 = 256;
/// `stress-ng --vm-bytes` value. Leaves ~100 MiB of headroom
/// under MEM_LIMIT_MIB for the guest agent + kernel — measured
/// requirement is ~56 MiB on alpine + boxlite v0.9.5 (cgroup hit
/// when alloc=200 MiB), so 100 MiB is comfortable. If a future
/// kernel/agent bloats past 100 MiB the cgroup ceiling fires and
/// `mem_pressure_exit_code` flips from 0 to non-zero — a clear
/// regression signal.
const MEM_ALLOC_MIB: u32 = 150;

/// Drive `apk add --no-cache stress-ng` exactly once per Scenario
/// instance. Result cached in the box's COW overlay for subsequent
/// iterations via shared `--home`.
async fn ensure_stress_ng(live: &LiteBox) -> Result<()> {
    let install = BoxCommand::new("apk").args(["add", "--no-cache", "stress-ng"]);
    let mut exec = live.exec(install).await.context("apk add stress-ng")?;
    if let Some(mut s) = exec.stdout() {
        tokio::spawn(async move { while s.next().await.is_some() {} });
    }
    if let Some(mut s) = exec.stderr() {
        tokio::spawn(async move { while s.next().await.is_some() {} });
    }
    let r = exec.wait().await.context("apk add stress-ng wait")?;
    if r.exit_code != 0 {
        anyhow::bail!("apk add stress-ng failed (exit {})", r.exit_code);
    }
    Ok(())
}

// ─── resource-cpu-load ─────────────────────────────────────────────

pub struct CpuLoad {
    home: Option<TempDir>,
    stress_installed: bool,
}

impl CpuLoad {
    pub fn new() -> Self {
        Self {
            home: None,
            stress_installed: false,
        }
    }
}

#[async_trait]
impl Scenario for CpuLoad {
    fn name(&self) -> &str {
        "resource-cpu-load"
    }

    async fn run_once(&mut self, ctx: &RunContext) -> Result<BTreeMap<String, f64>> {
        if self.home.is_none() {
            self.home = Some(TempDir::new().context("mkdir cpu-load home")?);
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

        if !self.stress_installed {
            ensure_stress_ng(&live).await?;
            self.stress_installed = true;
        }

        // Background the stress-ng exec — we sample BoxMetrics
        // while it runs.
        let stress_cmd = BoxCommand::new("stress-ng").args([
            "--cpu",
            "1",
            "--timeout",
            &format!("{CPU_LOAD_SECS}s"),
        ]);
        let mut stress_exec = live.exec(stress_cmd).await.context("box.exec(stress-ng)")?;
        if let Some(mut s) = stress_exec.stdout() {
            tokio::spawn(async move { while s.next().await.is_some() {} });
        }
        if let Some(mut s) = stress_exec.stderr() {
            tokio::spawn(async move { while s.next().await.is_some() {} });
        }

        // Brief settle so stress-ng's vCPUs are actually saturated
        // when the first sample fires (otherwise sample 1 catches
        // the warmup and skews `cpu_load_pct_mean` low).
        tokio::time::sleep(Duration::from_millis(500)).await;

        let mut rss_samples: Vec<u64> = Vec::new();
        let mut cpu_samples: Vec<f32> = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(CPU_LOAD_SECS - 1);
        while Instant::now() < deadline {
            let snap = live
                .metrics()
                .await
                .context("snap BoxMetrics under cpu load")?;
            if let Some(r) = snap.memory_bytes() {
                rss_samples.push(r);
            }
            if let Some(c) = snap.cpu_percent() {
                cpu_samples.push(c);
            }
            tokio::time::sleep(SAMPLE_INTERVAL).await;
        }

        let _ = tokio::time::timeout(Duration::from_secs(5), stress_exec.wait()).await;

        let mut metrics = BTreeMap::new();
        if !rss_samples.is_empty() {
            let max = rss_samples.iter().copied().max().unwrap_or(0);
            metrics.insert("rss_load_max_bytes".into(), max as f64);
        }
        if !cpu_samples.is_empty() {
            let max = cpu_samples
                .iter()
                .copied()
                .fold(f32::NEG_INFINITY, f32::max);
            let mean: f32 = cpu_samples.iter().copied().sum::<f32>() / cpu_samples.len() as f32;
            metrics.insert("cpu_load_pct_max".into(), max as f64);
            metrics.insert("cpu_load_pct_mean".into(), mean as f64);
        }
        metrics.insert("cpu_load_samples_count".into(), rss_samples.len() as f64);

        live.stop().await.context("box.stop()")?;
        guard.disarm();
        Ok(metrics)
    }
}

// ─── resource-mem-pressure ─────────────────────────────────────────

pub struct MemPressure {
    home: Option<TempDir>,
    stress_installed: bool,
}

impl MemPressure {
    pub fn new() -> Self {
        Self {
            home: None,
            stress_installed: false,
        }
    }
}

#[async_trait]
impl Scenario for MemPressure {
    fn name(&self) -> &str {
        "resource-mem-pressure"
    }

    async fn run_once(&mut self, ctx: &RunContext) -> Result<BTreeMap<String, f64>> {
        if self.home.is_none() {
            self.home = Some(TempDir::new().context("mkdir mem-pressure home")?);
        }
        let home_path = self
            .home
            .as_ref()
            .expect("just initialized")
            .path()
            .to_path_buf();
        let rt = build_runtime(ctx.global, home_path)?;

        // Override default memory so we have a known ceiling.
        let opts = BoxOptions {
            rootfs: RootfsSpec::Image("alpine:latest".into()),
            auto_remove: true,
            memory_mib: Some(MEM_LIMIT_MIB),
            ..Default::default()
        };
        let live = rt.create(opts, None).await.context("rt.create(alpine)")?;
        let mut guard = BoxGuard::new(&rt, live.id().to_string());
        live.start().await.context("box.start()")?;

        if !self.stress_installed {
            ensure_stress_ng(&live).await?;
            self.stress_installed = true;
        }

        // stress-ng's worker allocates, dirties, releases, repeats.
        // We don't use `--vm-keep` (which holds the alloc steady
        // across the run) because alpine's stress-ng 0.x exits
        // non-zero with that flag even on a clean cgroup-fit run,
        // which would make `mem_pressure_exit_code` useless as a
        // 0=clean signal. With the default churn cycle the worker
        // still hits peak RSS within the first second, so our
        // 2 s-cadence sample loop catches the high-water mark.
        let stress_cmd = BoxCommand::new("stress-ng").args([
            "--vm",
            "1",
            "--vm-bytes",
            &format!("{MEM_ALLOC_MIB}m"),
            "--timeout",
            &format!("{MEM_PRESSURE_SECS}s"),
        ]);
        let mut stress_exec = live.exec(stress_cmd).await.context("box.exec(stress-ng)")?;
        if let Some(mut s) = stress_exec.stdout() {
            tokio::spawn(async move { while s.next().await.is_some() {} });
        }
        if let Some(mut s) = stress_exec.stderr() {
            tokio::spawn(async move { while s.next().await.is_some() {} });
        }

        tokio::time::sleep(Duration::from_millis(500)).await;

        let mut rss_samples: Vec<u64> = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(MEM_PRESSURE_SECS - 1);
        while Instant::now() < deadline {
            let snap = live
                .metrics()
                .await
                .context("snap BoxMetrics under mem pressure")?;
            if let Some(r) = snap.memory_bytes() {
                rss_samples.push(r);
            }
            tokio::time::sleep(SAMPLE_INTERVAL).await;
        }

        // The headline: did stress-ng complete inside the cgroup
        // ceiling, or did the kernel OOM-kill it? Non-zero exit ==
        // pressure ceiling hit.
        let exit_code =
            match tokio::time::timeout(Duration::from_secs(10), stress_exec.wait()).await {
                Ok(Ok(r)) => r.exit_code as f64,
                Ok(Err(e)) => {
                    // wait error — surface as -1 so the report still
                    // captures something, but with `--threshold` checks
                    // a deviation from 0 will fail loudly.
                    eprintln!("stress-ng wait error: {e:#}");
                    -1.0
                }
                Err(_) => {
                    // Hit the 10s wait timeout. stress-ng is supposed
                    // to exit after MEM_PRESSURE_SECS; if it doesn't,
                    // either the box is hung (OOM-deadlock?) or
                    // boxlite's exec_wait is failing to deliver the
                    // result.
                    eprintln!("stress-ng wait timed out");
                    -2.0
                }
            };

        let mut metrics = BTreeMap::new();
        if !rss_samples.is_empty() {
            let max = rss_samples.iter().copied().max().unwrap_or(0);
            metrics.insert("rss_pressure_max_bytes".into(), max as f64);
        }
        metrics.insert("mem_pressure_exit_code".into(), exit_code);
        metrics.insert(
            "mem_pressure_limit_bytes".into(),
            (MEM_LIMIT_MIB as u64 * 1024 * 1024) as f64,
        );
        metrics.insert(
            "mem_pressure_alloc_bytes".into(),
            (MEM_ALLOC_MIB as u64 * 1024 * 1024) as f64,
        );

        live.stop().await.context("box.stop()")?;
        guard.disarm();
        Ok(metrics)
    }
}

//! Multi-vCPU CPU saturation — distinct from `resource-cpu-load`
//! (which pegs ONE vCPU). Tests how the libkrun shim handles a
//! box allocating multiple vCPUs and saturating all of them
//! simultaneously: vCPU thread scheduling overhead, KVM exit
//! batching, host CPU contention with the bench process itself.
//!
//! Per iteration:
//!   1. Create box with `cpus = VCPU_COUNT`.
//!   2. `apk add stress-ng` (cached on COW across iterations).
//!   3. `stress-ng --cpu N --timeout 10s` pegs all vCPUs.
//!   4. Sample BoxMetrics every 2 s.
//!
//! Reports:
//!   * `multi_vcpu_count` — VCPU_COUNT const.
//!   * `multi_vcpu_cpu_pct_mean` — should approach `100 *
//!     VCPU_COUNT` if libkrun's vCPU mapping is clean and the
//!     host has enough cores.
//!   * `multi_vcpu_rss_max_bytes` — peak RSS under saturated
//!     load. Grows with vCPU count (each vCPU has its own
//!     thread + stack in the shim).

use super::super::runner::{RunContext, Scenario};
use super::common::{BoxGuard, build_runtime};
use anyhow::{Context, Result};
use async_trait::async_trait;
use boxlite::BoxOptions;
use boxlite::runtime::options::RootfsSpec;
use boxlite::{BoxCommand, LiteBox};
use futures::StreamExt;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};
use tempfile::TempDir;

const VCPU_COUNT: u8 = 4;
const LOAD_SECS: u64 = 10;
const SAMPLE_INTERVAL: Duration = Duration::from_secs(2);

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
        anyhow::bail!("apk add stress-ng failed exit {}", r.exit_code);
    }
    Ok(())
}

pub struct MultiVcpu {
    home: Option<TempDir>,
    stress_installed: bool,
}

impl MultiVcpu {
    pub fn new() -> Self {
        Self {
            home: None,
            stress_installed: false,
        }
    }
}

#[async_trait]
impl Scenario for MultiVcpu {
    fn name(&self) -> &str {
        "resource-multi-vcpu-load"
    }

    async fn run_once(&mut self, ctx: &RunContext) -> Result<BTreeMap<String, f64>> {
        if self.home.is_none() {
            self.home = Some(TempDir::new().context("mkdir multi-vcpu home")?);
        }
        let home_path = self
            .home
            .as_ref()
            .expect("just initialized")
            .path()
            .to_path_buf();
        let rt = build_runtime(ctx.global, home_path)?;

        let opts = BoxOptions {
            rootfs: RootfsSpec::Image("alpine:latest".into()),
            auto_remove: true,
            cpus: Some(VCPU_COUNT),
            ..Default::default()
        };
        let live = rt.create(opts, None).await.context("rt.create(alpine)")?;
        let mut guard = BoxGuard::new(&rt, live.id().to_string());
        live.start().await.context("box.start()")?;

        if !self.stress_installed {
            ensure_stress_ng(&live).await?;
            self.stress_installed = true;
        }

        let stress_cmd = BoxCommand::new("stress-ng").args([
            "--cpu",
            &VCPU_COUNT.to_string(),
            "--timeout",
            &format!("{LOAD_SECS}s"),
        ]);
        let mut stress_exec = live
            .exec(stress_cmd)
            .await
            .context("box.exec(stress-ng --cpu N)")?;
        if let Some(mut s) = stress_exec.stdout() {
            tokio::spawn(async move { while s.next().await.is_some() {} });
        }
        if let Some(mut s) = stress_exec.stderr() {
            tokio::spawn(async move { while s.next().await.is_some() {} });
        }

        tokio::time::sleep(Duration::from_millis(500)).await;

        let mut cpu_samples: Vec<f32> = Vec::new();
        let mut rss_samples: Vec<u64> = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(LOAD_SECS - 1);
        while Instant::now() < deadline {
            let snap = live.metrics().await.context("snap BoxMetrics")?;
            if let Some(c) = snap.cpu_percent() {
                cpu_samples.push(c);
            }
            if let Some(r) = snap.memory_bytes() {
                rss_samples.push(r);
            }
            tokio::time::sleep(SAMPLE_INTERVAL).await;
        }

        let _ = tokio::time::timeout(Duration::from_secs(5), stress_exec.wait()).await;

        let mut metrics = BTreeMap::new();
        metrics.insert("multi_vcpu_count".into(), VCPU_COUNT as f64);
        if !cpu_samples.is_empty() {
            let mean: f32 = cpu_samples.iter().copied().sum::<f32>() / cpu_samples.len() as f32;
            let max = cpu_samples
                .iter()
                .copied()
                .fold(f32::NEG_INFINITY, f32::max);
            metrics.insert("multi_vcpu_cpu_pct_mean".into(), mean as f64);
            metrics.insert("multi_vcpu_cpu_pct_max".into(), max as f64);
        }
        if let Some(peak) = rss_samples.iter().max() {
            metrics.insert("multi_vcpu_rss_max_bytes".into(), *peak as f64);
        }

        live.stop().await.context("box.stop()")?;
        guard.disarm();
        Ok(metrics)
    }
}

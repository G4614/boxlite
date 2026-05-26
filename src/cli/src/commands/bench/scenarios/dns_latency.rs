//! In-box DNS lookup latency via gvproxy's embedded resolver.
//!
//! gvproxy runs an embedded DNS server on 192.168.127.1:53 — the
//! guest's `/etc/resolv.conf` points there by default. This
//! scenario uses `getent ahosts <name>` inside the box to time
//! lookups: getent is busybox-built-in, no install needed.
//!
//! Two targets, both relevant:
//!   * `local`: a hostname gvproxy can answer from its built-in
//!     `dns_zones` (e.g., `host.docker.internal`-style local
//!     mappings). Tests the in-process answer path.
//!   * `external`: a real internet hostname forwarded upstream
//!     (`example.com`). Tests the recursive-forward path. Falls
//!     back gracefully if there's no internet (recorded as
//!     `dns_external_failed_count`).
//!
//! N=20 sequential lookups per target; report mean, max, and
//! failure count per side.

use super::super::runner::{RunContext, Scenario};
use super::common::{BoxGuard, alpine_options, build_runtime};
use anyhow::{Context, Result};
use async_trait::async_trait;
use boxlite::BoxCommand;
use futures::StreamExt;
use std::collections::BTreeMap;
use std::time::Instant;
use tempfile::TempDir;

const N_LOOKUPS: usize = 20;
const LOCAL_TARGET: &str = "host.docker.internal";
const EXTERNAL_TARGET: &str = "example.com";

pub struct DnsLatency {
    home: Option<TempDir>,
}

impl DnsLatency {
    pub fn new() -> Self {
        Self { home: None }
    }
}

#[async_trait]
impl Scenario for DnsLatency {
    fn name(&self) -> &str {
        "throughput-dns-latency"
    }

    async fn run_once(&mut self, ctx: &RunContext) -> Result<BTreeMap<String, f64>> {
        if self.home.is_none() {
            self.home = Some(TempDir::new().context("mkdir dns-latency home")?);
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

        let (local_mean, local_max, local_fail) = measure_lookups(&live, LOCAL_TARGET).await?;
        let (ext_mean, ext_max, ext_fail) = measure_lookups(&live, EXTERNAL_TARGET).await?;

        live.stop().await.context("box.stop()")?;
        guard.disarm();

        let mut metrics = BTreeMap::new();
        metrics.insert("dns_local_mean_ms".into(), local_mean);
        metrics.insert("dns_local_max_ms".into(), local_max);
        metrics.insert("dns_local_failed_count".into(), local_fail as f64);
        metrics.insert("dns_external_mean_ms".into(), ext_mean);
        metrics.insert("dns_external_max_ms".into(), ext_max);
        metrics.insert("dns_external_failed_count".into(), ext_fail as f64);
        metrics.insert("dns_lookups_per_target_count".into(), N_LOOKUPS as f64);
        Ok(metrics)
    }
}

/// Fire N_LOOKUPS sequential `getent ahosts <target>` and time
/// each. Each call goes through nsswitch → DNS, hitting gvproxy's
/// resolver. Returns `(mean_ms, max_ms, failed_count)`.
async fn measure_lookups(live: &boxlite::LiteBox, target: &str) -> Result<(f64, f64, usize)> {
    let mut times = Vec::with_capacity(N_LOOKUPS);
    let mut failed = 0usize;
    for _ in 0..N_LOOKUPS {
        let cmd = BoxCommand::new("getent").args(["ahosts", target]);
        let start = Instant::now();
        let mut exec = live.exec(cmd).await.context("box.exec(getent)")?;
        if let Some(mut s) = exec.stdout() {
            tokio::spawn(async move { while s.next().await.is_some() {} });
        }
        if let Some(mut s) = exec.stderr() {
            tokio::spawn(async move { while s.next().await.is_some() {} });
        }
        let r = exec.wait().await.context("getent wait")?;
        let dur_ms = start.elapsed().as_secs_f64() * 1000.0;
        if r.exit_code == 0 {
            times.push(dur_ms);
        } else {
            failed += 1;
        }
    }
    if times.is_empty() {
        return Ok((0.0, 0.0, failed));
    }
    let mean = times.iter().copied().sum::<f64>() / times.len() as f64;
    let max = times.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    Ok((mean, max, failed))
}

//! gvproxy port-table cost at fan-out.
//!
//! `throughput-net-tcp-sink` exercises one host→guest forward. Real
//! deployments commonly publish many ports on one box (HTTP, gRPC,
//! metrics, debug, ...). gvproxy stitches each one into its
//! userspace netstack; the per-port setup cost shows up as box
//! create+start latency growing with N.
//!
//! This scenario creates a box with N=16 `-p host:guest` forwards
//! (host_port=None → OS-ephemeral), starts it, and reports:
//!   * `many_ports_n` — N (always 16).
//!   * `many_ports_create_ms` — `rt.create` wall.
//!   * `many_ports_start_ms` — `box.start` wall.
//!
//! Delta against `latency-warm-start` (which uses zero ports) is the
//! per-port amortized cost.

use super::super::runner::{RunContext, Scenario};
use super::common::{BoxGuard, alpine_options, build_runtime};
use anyhow::{Context, Result};
use async_trait::async_trait;
use boxlite::runtime::options::PortSpec;
use std::collections::BTreeMap;
use std::time::Instant;
use tempfile::TempDir;

const N_PORTS: u16 = 16;
/// Guest-side base port. Picked to avoid common in-image services
/// (sshd 22, alpine's default cron's lockfile dir, etc.).
const GUEST_BASE: u16 = 18000;

pub struct ManyPorts {
    home: Option<TempDir>,
}

impl ManyPorts {
    pub fn new() -> Self {
        Self { home: None }
    }
}

#[async_trait]
impl Scenario for ManyPorts {
    fn name(&self) -> &str {
        "throughput-many-ports-setup"
    }

    async fn run_once(&mut self, ctx: &RunContext) -> Result<BTreeMap<String, f64>> {
        if self.home.is_none() {
            self.home = Some(TempDir::new().context("mkdir many-ports home")?);
        }
        let home_path = self
            .home
            .as_ref()
            .expect("just initialized")
            .path()
            .to_path_buf();
        let rt = build_runtime(ctx.global, home_path)?;

        let mut opts = alpine_options();
        opts.ports = (0..N_PORTS)
            .map(|i| PortSpec {
                host_port: None,
                guest_port: GUEST_BASE + i,
                ..PortSpec::default()
            })
            .collect();

        let t_create = Instant::now();
        let live = rt
            .create(opts, None)
            .await
            .context("rt.create(many ports)")?;
        let create_ms = t_create.elapsed().as_secs_f64() * 1000.0;
        let mut guard = BoxGuard::new(&rt, live.id().to_string());

        let t_start = Instant::now();
        live.start().await.context("box.start")?;
        let start_ms = t_start.elapsed().as_secs_f64() * 1000.0;

        live.stop().await.context("box.stop")?;
        guard.disarm();

        let mut metrics = BTreeMap::new();
        metrics.insert("many_ports_n".into(), N_PORTS as f64);
        metrics.insert("many_ports_create_ms".into(), create_ms);
        metrics.insert("many_ports_start_ms".into(), start_ms);
        Ok(metrics)
    }
}

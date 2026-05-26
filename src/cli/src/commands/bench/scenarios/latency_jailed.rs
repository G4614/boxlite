//! Cold-start with the maximum-isolation security profile.
//!
//! `latency-cold-start` measures the cheapest cold-start path — the
//! defaults (jailer disabled on Linux, no seccomp, no UID drop). This
//! scenario re-runs cold-start with `SecurityOptions::maximum()`:
//! jailer + seccomp + UID/GID drop + new PID NS + chroot + close_fds +
//! sanitize_env + rlimits. The delta between the two cold-starts is
//! the per-box isolation tax that production deployments pay.
//!
//! Two SKIP gates:
//!   1. `SecurityOptions::is_full_isolation_available()` — non-Linux
//!      platforms can't honor the full profile, so the headline
//!      number would be meaningless.
//!   2. `bwrap --unshare-user` preflight — on Ubuntu 24+ AppArmor
//!      restricts unprivileged user namespaces by default. The
//!      `box.start()` would fail with a Permission-denied during
//!      `bwrap`'s setuid-map and the scenario can't measure
//!      anything. Detected before create() so the report carries
//!      `cold_jailed_skipped=1` instead of a runner failure.

use super::super::runner::{RunContext, Scenario};
use super::common::{BoxGuard, alpine_options, build_runtime};
use anyhow::{Context, Result};
use async_trait::async_trait;
use boxlite::runtime::advanced_options::{AdvancedBoxOptions, SecurityOptions};
use std::collections::BTreeMap;
use std::process::Command;
use tempfile::TempDir;

/// Probe whether `bwrap --unshare-user` can actually create a user
/// namespace on this host. Used as a preflight for the jailed
/// cold-start; returns false on Ubuntu 24+ when AppArmor restricts
/// unprivileged user-namespace creation by default.
fn bwrap_userns_available() -> bool {
    Command::new("bwrap")
        .args(["--unshare-user", "--bind", "/", "/", "true"])
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

pub struct LatencyColdStartJailed;

impl LatencyColdStartJailed {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Scenario for LatencyColdStartJailed {
    fn name(&self) -> &str {
        "latency-cold-start-jailed"
    }

    async fn run_once(&mut self, ctx: &RunContext) -> Result<BTreeMap<String, f64>> {
        if !SecurityOptions::is_full_isolation_available() {
            eprintln!(
                "SKIP latency-cold-start-jailed: \
                 SecurityOptions::is_full_isolation_available()=false (non-Linux)"
            );
            let mut out = BTreeMap::new();
            out.insert("cold_jailed_skipped".into(), 1.0);
            return Ok(out);
        }
        if !bwrap_userns_available() {
            eprintln!(
                "SKIP latency-cold-start-jailed: `bwrap --unshare-user` preflight failed \
                 (likely AppArmor restricting unprivileged userns; see \
                 kernel.apparmor_restrict_unprivileged_userns)."
            );
            let mut out = BTreeMap::new();
            out.insert("cold_jailed_skipped".into(), 1.0);
            return Ok(out);
        }

        let home = TempDir::new().context("mkdir cold-jailed home")?;
        let home_path = home.path().to_path_buf();
        let rt = build_runtime(ctx.global, home_path)?;

        let mut opts = alpine_options();
        opts.advanced = AdvancedBoxOptions {
            security: SecurityOptions::maximum(),
            ..AdvancedBoxOptions::default()
        };

        let live = rt
            .create(opts, None)
            .await
            .context("rt.create(jailed alpine)")?;
        let mut guard = BoxGuard::new(&rt, live.id().to_string());
        live.start().await.context("box.start()")?;

        let mut metrics = BTreeMap::new();
        let m = live.metrics().await.context("snapshot BoxMetrics")?;
        if let Some(v) = m.total_create_duration_ms() {
            metrics.insert("total_create_ms".into(), v as f64);
        }
        if let Some(v) = m.guest_boot_duration_ms() {
            metrics.insert("guest_boot_ms".into(), v as f64);
        }
        if let Some(v) = m.stage_filesystem_setup_ms() {
            metrics.insert("stage_filesystem_setup_ms".into(), v as f64);
        }
        if let Some(v) = m.stage_image_prepare_ms() {
            metrics.insert("stage_image_prepare_ms".into(), v as f64);
        }
        if let Some(v) = m.stage_guest_rootfs_ms() {
            metrics.insert("stage_guest_rootfs_ms".into(), v as f64);
        }
        if let Some(v) = m.stage_box_config_ms() {
            metrics.insert("stage_box_config_ms".into(), v as f64);
        }
        if let Some(v) = m.stage_box_spawn_ms() {
            metrics.insert("stage_box_spawn_ms".into(), v as f64);
        }
        if let Some(v) = m.stage_container_init_ms() {
            metrics.insert("stage_container_init_ms".into(), v as f64);
        }

        live.stop().await.context("box.stop()")?;
        guard.disarm();
        // home dropped at end of fn → fresh per iteration (cold).
        Ok(metrics)
    }
}

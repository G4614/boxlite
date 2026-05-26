//! Create+start cost with N=2 host-volume mounts.
//!
//! `throughput-virtiofs` measures bandwidth through one mount.
//! This scenario answers a different question: what does setting
//! up the box cost when multiple mounts are pre-declared? Each mount
//! adds a virtiofs server, a guest-side mount-table entry, and a
//! tagged share — the per-mount setup cost shows up at create+start
//! time, not at I/O time.
//!
//! N is hard-capped at 2 — libkrun's `KRUN_VIRTIO_FS_MAX` on this
//! build is 2; declaring 3+ mounts at box-create time fails with
//! `libkrun status=-22` at `start()`. The "many-ports" scenario hits
//! 16 forwards cleanly because gvproxy's port table isn't bounded
//! the same way. Single vs double mount still surfaces a measurable
//! per-mount tax — extending past 2 would need either a different
//! VMM or a sharded-virtiofs change in boxlite.
//!
//! Reports:
//!   * `volumes_n` — N (always 2).
//!   * `volumes_create_ms` — `rt.create` wall.
//!   * `volumes_start_ms` — `box.start` wall.
//!
//! Delta against `throughput-virtiofs` (1 mount) is the per-mount
//! amortized setup tax.

use super::super::runner::{RunContext, Scenario};
use super::common::{BoxGuard, alpine_options, build_runtime};
use anyhow::{Context, Result};
use async_trait::async_trait;
use boxlite::runtime::options::VolumeSpec;
use std::collections::BTreeMap;
use std::time::Instant;
use tempfile::TempDir;

const N_MOUNTS: usize = 2;

pub struct VolumesMulti {
    home: Option<TempDir>,
    /// Pre-allocated host dirs for the mounts; reused across iterations
    /// so disk doesn't grow.
    host_dirs: Vec<TempDir>,
    prewarmed: bool,
}

impl VolumesMulti {
    pub fn new() -> Self {
        Self {
            home: None,
            host_dirs: Vec::new(),
            prewarmed: false,
        }
    }
}

#[async_trait]
impl Scenario for VolumesMulti {
    fn name(&self) -> &str {
        "throughput-volumes-multi-setup"
    }

    async fn run_once(&mut self, ctx: &RunContext) -> Result<BTreeMap<String, f64>> {
        if self.home.is_none() {
            self.home = Some(TempDir::new().context("mkdir volumes home")?);
        }
        if self.host_dirs.is_empty() {
            for i in 0..N_MOUNTS {
                self.host_dirs
                    .push(TempDir::new().with_context(|| format!("mkdir host vol #{i}"))?);
            }
        }
        let home_path = self
            .home
            .as_ref()
            .expect("just initialized")
            .path()
            .to_path_buf();
        let rt = build_runtime(ctx.global, home_path)?;

        // First-call pre-warm: one throwaway box without volumes so
        // image + base disk are local before the timed mount cycle.
        // Without this, `volumes_start_ms` on the first iter would
        // be dominated by image pull instead of virtiofs fan-out.
        if !self.prewarmed {
            let warm = rt
                .create(alpine_options(), None)
                .await
                .context("volumes pre-warm create")?;
            warm.start().await.context("volumes pre-warm start")?;
            warm.stop().await.context("volumes pre-warm stop")?;
            self.prewarmed = true;
        }

        let mut opts = alpine_options();
        opts.volumes = self
            .host_dirs
            .iter()
            .enumerate()
            .map(|(i, d)| VolumeSpec {
                host_path: d.path().to_string_lossy().into_owned(),
                guest_path: format!("/mnt/host{i}"),
                read_only: false,
            })
            .collect();

        let t_create = Instant::now();
        let live = rt.create(opts, None).await.context("rt.create(volumes)")?;
        let create_ms = t_create.elapsed().as_secs_f64() * 1000.0;
        let mut guard = BoxGuard::new(&rt, live.id().to_string());

        let t_start = Instant::now();
        live.start().await.context("box.start")?;
        let start_ms = t_start.elapsed().as_secs_f64() * 1000.0;

        live.stop().await.context("box.stop")?;
        guard.disarm();

        let mut metrics = BTreeMap::new();
        metrics.insert("volumes_n".into(), N_MOUNTS as f64);
        metrics.insert("volumes_create_ms".into(), create_ms);
        metrics.insert("volumes_start_ms".into(), start_ms);
        Ok(metrics)
    }
}

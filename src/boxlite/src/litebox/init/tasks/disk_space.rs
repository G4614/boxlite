//! Task: host disk-space admission guard.
//!
//! Runs first, before any rootfs/image work touches the disk. Refuses to start
//! when the host filesystem is critically low (writes would fail mid-operation)
//! and warns when it is getting low. This is admission control only — it does
//! not bound how much a running box can write (volumes have no quota).

use super::{InitCtx, log_task_error, task_start};
use crate::pipeline::PipelineTask;
use crate::runtime::gc::GcOptions;
use crate::util::{DiskSpaceVerdict, available_and_total, classify};
use async_trait::async_trait;
use boxlite_shared::errors::{BoxliteError, BoxliteResult};

pub struct DiskSpaceTask;

#[async_trait]
impl PipelineTask<InitCtx> for DiskSpaceTask {
    async fn run(self: Box<Self>, ctx: InitCtx) -> BoxliteResult<()> {
        let task_name = self.name();
        let box_id = task_start(&ctx, task_name).await;

        let (home_dir, runtime) = {
            let ctx = ctx.lock().await;
            (
                ctx.runtime.layout.home_dir().to_path_buf(),
                ctx.runtime.clone(),
            )
        };

        // Failing to read free space must not block startup — degrade to a
        // warning. The guard is a safety net, not a hard dependency.
        let Some(initial) = read_free(&home_dir, &box_id) else {
            return Ok(());
        };

        let (final_verdict, result) = decide_admission(
            initial,
            || read_free(&home_dir, &box_id),
            || {
                tracing::warn!(box_id = %box_id, "Disk pressure at box start — running cache GC");
                match runtime.collect_garbage(&GcOptions::default()) {
                    Ok(report) => tracing::info!(
                        box_id = %box_id,
                        reclaimed_bytes = report.total_bytes(),
                        "Cache GC freed space before admission"
                    ),
                    Err(e) => tracing::warn!(box_id = %box_id, error = %e, "Cache GC failed"),
                }
            },
        );

        if let DiskSpaceVerdict::Warn(msg) = &final_verdict {
            tracing::warn!(box_id = %box_id, "{msg}");
        }
        if let Err(e) = &result {
            log_task_error(&box_id, task_name, e);
        }
        result
    }

    fn name(&self) -> &str {
        "disk_space_guard"
    }
}

/// statvfs the home filesystem; `None` (with a warning) if it can't be read,
/// so a probe failure degrades to "allow start" rather than blocking it.
fn read_free(home_dir: &std::path::Path, box_id: &crate::BoxID) -> Option<(u64, u64)> {
    match available_and_total(home_dir) {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!(box_id = %box_id, path = %home_dir.display(), error = %e,
                "Could not read host free space; skipping disk-space guard");
            None
        }
    }
}

/// Pure admission decision over the classify/GC-retry cascade, so the Reject
/// path can be exercised without a real low-disk filesystem.
///
/// - Ok → admit, GC not invoked.
/// - Warn or Reject → invoke `run_gc`, re-read free space, re-classify.
/// - Final Ok / Warn → admit (caller logs the Warn message).
/// - Final Reject → `Err(ResourceExhausted)`.
///
/// Returns both the final verdict (so the caller can log a Warn) and the
/// admission result.
fn decide_admission(
    initial: (u64, u64),
    re_read: impl FnOnce() -> Option<(u64, u64)>,
    run_gc: impl FnOnce(),
) -> (DiskSpaceVerdict, BoxliteResult<()>) {
    let mut verdict = classify(initial.0, initial.1);
    // Under any non-Ok pressure, try GC and re-classify before deciding.
    if !matches!(verdict, DiskSpaceVerdict::Ok) {
        run_gc();
        if let Some((free, total)) = re_read() {
            verdict = classify(free, total);
        }
    }
    let result = match &verdict {
        DiskSpaceVerdict::Ok | DiskSpaceVerdict::Warn(_) => Ok(()),
        DiskSpaceVerdict::Reject(msg) => Err(BoxliteError::ResourceExhausted(msg.clone())),
    };
    (verdict, result)
}

#[cfg(test)]
mod tests {
    use super::{DiskSpaceTask, decide_admission};
    use crate::litebox::config::{BoxConfig, ContainerRuntimeConfig};
    use crate::litebox::init::tasks::InitCtx;
    use crate::litebox::init::types::InitPipelineContext;
    use crate::pipeline::PipelineTask;
    use crate::runtime::id::BoxID;
    use crate::runtime::options::{BoxOptions, BoxliteOptions, RootfsSpec};
    use crate::runtime::rt_impl::RuntimeImpl;
    use crate::runtime::types::ContainerID;
    use crate::util::{DiskSpaceVerdict, available_and_total, classify};
    use crate::vmm::VmmKind;
    use boxlite_shared::Transport;
    use boxlite_shared::errors::BoxliteError;
    use boxlite_test_utils::home::PerTestBoxHome;
    use chrono::Utc;
    use std::cell::Cell;
    use std::path::PathBuf;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    /// Task-level integration: `run()` reads the box's real home from the
    /// pipeline context, statvfs's it, and maps the verdict to its `Result`.
    ///
    /// Asserting against `classify()` on the home's *actual* free space (rather
    /// than a fixed expectation) keeps this non-flaky regardless of how much
    /// disk the test host has — it proves the wiring, not a disk size.
    #[tokio::test]
    async fn run_matches_classify_verdict_for_real_home() {
        let home = PerTestBoxHome::isolated_in("/tmp");
        let runtime = RuntimeImpl::new(BoxliteOptions {
            home_dir: home.path.clone(),
            image_registries: vec![],
        })
        .expect("create runtime");

        let config = BoxConfig {
            id: BoxID::parse("01HJK4TNRPQSXYZ8WM6NCVT9CG1").unwrap(),
            name: None,
            created_at: Utc::now(),
            container: ContainerRuntimeConfig {
                id: ContainerID::new(),
            },
            options: BoxOptions {
                rootfs: RootfsSpec::Image("test:latest".to_string()),
                ..Default::default()
            },
            engine_kind: VmmKind::Libkrun,
            transport: Transport::unix(PathBuf::from("/tmp/test.sock")),
            box_home: PathBuf::from("/tmp/box"),
            ready_socket_path: PathBuf::from("/tmp/ready"),
        };

        // Expected verdict from the SAME home the task will inspect.
        let home_dir = runtime.layout.home_dir().to_path_buf();
        let (free, total) = available_and_total(&home_dir).expect("statvfs test home");
        let expected = classify(free, total);

        let ctx: InitCtx = Arc::new(Mutex::new(InitPipelineContext::new(
            config, runtime, false, false,
        )));

        let result = Box::new(DiskSpaceTask).run(ctx.clone()).await;

        // The box was never seeded into the manager; stop CleanupGuard's Drop
        // from trying to mark a nonexistent box Failed.
        ctx.lock().await.guard.disarm();

        match expected {
            DiskSpaceVerdict::Reject(_) => {
                assert!(result.is_err(), "a rejecting home must block startup")
            }
            _ => assert!(
                result.is_ok(),
                "a non-rejecting home must start; got {result:?}"
            ),
        }
    }

    // ── Pure decision tests (cover the branches CI's healthy host can't trip) ──

    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * MIB;

    /// Free space already comfortable → admit immediately, GC must not run.
    #[test]
    fn decide_admission_admits_when_ok_without_running_gc() {
        let gc_calls = Cell::new(0u32);
        let (final_v, result) = decide_admission(
            (50 * GIB, 100 * GIB), // plenty
            || panic!("re_read must not be called when initial is Ok"),
            || gc_calls.set(gc_calls.get() + 1),
        );
        assert_eq!(final_v, DiskSpaceVerdict::Ok);
        assert!(result.is_ok());
        assert_eq!(
            gc_calls.get(),
            0,
            "GC must not run when free space is already Ok"
        );
    }

    /// Critically low → GC runs, still low → Reject → `ResourceExhausted`.
    /// Closes the production gap: this `Err` branch is the one a healthy CI
    /// host can never exercise via [`run_matches_classify_verdict_for_real_home`].
    #[test]
    fn decide_admission_rejects_when_pressure_persists_after_gc() {
        let gc_calls = Cell::new(0u32);
        let (final_v, result) = decide_admission(
            (100 * MIB, 100 * GIB), // below the 1 GiB hard floor
            || Some((100 * MIB, 100 * GIB)),
            || gc_calls.set(gc_calls.get() + 1),
        );
        assert!(
            matches!(final_v, DiskSpaceVerdict::Reject(_)),
            "got {final_v:?}"
        );
        assert!(
            matches!(result, Err(BoxliteError::ResourceExhausted(_))),
            "got {result:?}"
        );
        assert_eq!(
            gc_calls.get(),
            1,
            "GC must be tried under any non-Ok pressure"
        );
    }

    /// Critically low, but GC reclaims enough → admit. Proves the retry-after-GC
    /// path actually re-classifies (and a successful reclaim flips Reject to Ok).
    #[test]
    fn decide_admission_admits_when_gc_clears_pressure() {
        let gc_calls = Cell::new(0u32);
        let (final_v, result) = decide_admission(
            (100 * MIB, 100 * GIB),         // Reject
            || Some((50 * GIB, 100 * GIB)), // GC freed up plenty
            || gc_calls.set(gc_calls.get() + 1),
        );
        assert_eq!(final_v, DiskSpaceVerdict::Ok);
        assert!(result.is_ok());
        assert_eq!(gc_calls.get(), 1);
    }

    /// Soft-warn pressure must still admit (Warn is not a hard refusal), but
    /// GC is still tried first — proves GC fires on Warn, not only on Reject.
    #[test]
    fn decide_admission_admits_warn_after_gc_still_warn() {
        let gc_calls = Cell::new(0u32);
        let (final_v, result) = decide_admission(
            (3 * GIB, 1024 * GIB), // above hard, below soft → Warn
            || Some((3 * GIB, 1024 * GIB)),
            || gc_calls.set(gc_calls.get() + 1),
        );
        assert!(
            matches!(final_v, DiskSpaceVerdict::Warn(_)),
            "got {final_v:?}"
        );
        assert!(result.is_ok(), "Warn must not refuse a start");
        assert_eq!(
            gc_calls.get(),
            1,
            "GC must be attempted whenever the initial verdict is non-Ok"
        );
    }
}

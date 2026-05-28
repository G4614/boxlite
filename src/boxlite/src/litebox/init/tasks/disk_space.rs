//! Task: host disk-space admission guard.
//!
//! Runs first, before any rootfs/image work touches the disk. Refuses to start
//! when the host filesystem is critically low (writes would fail mid-operation)
//! and warns when it is getting low. This is admission control only — it does
//! not bound how much a running box can write (volumes have no quota).

use super::{InitCtx, log_task_error, task_start};
use crate::pipeline::PipelineTask;
use crate::util::{DiskSpaceVerdict, available_and_total, classify};
use async_trait::async_trait;
use boxlite_shared::errors::{BoxliteError, BoxliteResult};

pub struct DiskSpaceTask;

#[async_trait]
impl PipelineTask<InitCtx> for DiskSpaceTask {
    async fn run(self: Box<Self>, ctx: InitCtx) -> BoxliteResult<()> {
        let task_name = self.name();
        let box_id = task_start(&ctx, task_name).await;

        let home_dir = {
            let ctx = ctx.lock().await;
            ctx.runtime.layout.home_dir().to_path_buf()
        };

        // Failing to read free space must not block startup — degrade to a
        // warning. The guard is a safety net, not a hard dependency.
        let (free, total) = match available_and_total(&home_dir) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(box_id = %box_id, path = %home_dir.display(), error = %e,
                    "Could not read host free space; skipping disk-space guard");
                return Ok(());
            }
        };

        match classify(free, total) {
            DiskSpaceVerdict::Ok => {}
            DiskSpaceVerdict::Warn(msg) => {
                tracing::warn!(box_id = %box_id, "{msg}");
            }
            DiskSpaceVerdict::Reject(msg) => {
                let err = BoxliteError::ResourceExhausted(msg);
                log_task_error(&box_id, task_name, &err);
                return Err(err);
            }
        }

        Ok(())
    }

    fn name(&self) -> &str {
        "disk_space_guard"
    }
}

#[cfg(test)]
mod tests {
    use super::DiskSpaceTask;
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
    use boxlite_test_utils::home::PerTestBoxHome;
    use chrono::Utc;
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
}

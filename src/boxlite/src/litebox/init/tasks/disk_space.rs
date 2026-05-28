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

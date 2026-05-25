//! Runtime-level metrics (aggregate across all boxes).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Storage for runtime-wide metrics.
///
/// Stored in `RuntimeState`, shared across all operations.
/// All counters are monotonic (never decrease).
#[derive(Clone, Default)]
pub struct RuntimeMetricsStorage {
    /// Total boxes created since runtime startup
    pub(crate) boxes_created: Arc<AtomicU64>,
    /// Total boxes that failed to start
    pub(crate) boxes_failed: Arc<AtomicU64>,
    /// Total boxes stopped (explicitly or via shutdown)
    pub(crate) boxes_stopped: Arc<AtomicU64>,
    /// Total commands executed across all boxes
    pub(crate) total_commands: Arc<AtomicU64>,
    /// Total command execution errors across all boxes
    pub(crate) total_exec_errors: Arc<AtomicU64>,
    /// Total shim PIDs the scoped reaper has collected (Issue #523).
    /// Monotonic; tracks shim children whose `Child` handle was dropped
    /// without `wait()` — the load-bearing zombie-prevention signal.
    pub(crate) shim_reaped: Arc<AtomicU64>,
    /// Total shim PIDs that vanished out from under the reaper before
    /// it could collect them (ECHILD on waitpid). Monotonic. High vs
    /// `shim_reaped` ratio means most cleanups are handled by the
    /// owner's own `Child::wait()` — that's a healthy signal.
    pub(crate) shim_reaper_vanished: Arc<AtomicU64>,
    /// Current size of the reaper's PID registry. Gauge, not counter —
    /// this rises when a shim is registered, falls when reaper sweeps
    /// it. Sustained growth means either many running boxes or a
    /// reaper-side bug; flag in dashboards if it climbs without
    /// `shim_reaped` keeping pace.
    pub(crate) shim_reaper_registered: Arc<AtomicU64>,
}

impl RuntimeMetricsStorage {
    /// Create new runtime metrics storage.
    pub fn new() -> Self {
        Self::default()
    }
}

/// Handle for querying runtime-wide metrics.
///
/// Cloneable, lightweight handle (only Arc pointers).
/// All counters are monotonic and never reset.
#[derive(Clone)]
pub struct RuntimeMetrics {
    storage: RuntimeMetricsStorage,
}

impl RuntimeMetrics {
    /// Create new handle from storage.
    pub(crate) fn new(storage: RuntimeMetricsStorage) -> Self {
        Self { storage }
    }

    /// Total number of boxes created since runtime startup.
    ///
    /// Incremented when `BoxliteRuntime::create()` is called.
    /// Never decreases (monotonic counter).
    pub fn boxes_created_total(&self) -> u64 {
        self.storage.boxes_created.load(Ordering::Relaxed)
    }

    /// Total number of boxes that failed to start.
    ///
    /// Incremented when box creation or initialization fails.
    /// Never decreases (monotonic counter).
    pub fn boxes_failed_total(&self) -> u64 {
        self.storage.boxes_failed.load(Ordering::Relaxed)
    }

    /// Total number of boxes that have been stopped.
    ///
    /// Incremented when `LiteBox::stop()` completes successfully.
    /// Never decreases (monotonic counter).
    pub fn boxes_stopped_total(&self) -> u64 {
        self.storage.boxes_stopped.load(Ordering::Relaxed)
    }

    /// Number of currently running boxes.
    ///
    /// Calculated as: boxes_created - boxes_stopped - boxes_failed
    pub fn num_running_boxes(&self) -> u64 {
        let created = self.boxes_created_total();
        let stopped = self.boxes_stopped_total();
        let failed = self.boxes_failed_total();
        created.saturating_sub(stopped).saturating_sub(failed)
    }

    /// Total commands executed across all boxes.
    ///
    /// Incremented on every `LiteBox::exec()` call.
    /// Never decreases (monotonic counter).
    pub fn total_commands_executed(&self) -> u64 {
        self.storage.total_commands.load(Ordering::Relaxed)
    }

    /// Total command execution errors across all boxes.
    ///
    /// Incremented when `LiteBox::exec()` returns error.
    /// Never decreases (monotonic counter).
    pub fn total_exec_errors(&self) -> u64 {
        self.storage.total_exec_errors.load(Ordering::Relaxed)
    }

    /// Total shim PIDs the scoped reaper collected (Issue #523).
    /// Never decreases.
    pub fn shim_reaped_total(&self) -> u64 {
        self.storage.shim_reaped.load(Ordering::Relaxed)
    }

    /// Total shim PIDs that vanished (ECHILD) before the reaper saw
    /// them. Never decreases. See `shim_reaped` field doc on
    /// `RuntimeMetricsStorage` for what the ratio means operationally.
    pub fn shim_reaper_vanished_total(&self) -> u64 {
        self.storage.shim_reaper_vanished.load(Ordering::Relaxed)
    }

    /// Current size of the reaper's PID registry. Gauge.
    pub fn shim_reaper_registered(&self) -> u64 {
        self.storage.shim_reaper_registered.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_num_running_boxes_calculation() {
        let storage = RuntimeMetricsStorage::new();
        let metrics = RuntimeMetrics::new(storage.clone());

        // Initially all counters are 0
        assert_eq!(metrics.num_running_boxes(), 0);

        // Create 5 boxes
        for _ in 0..5 {
            storage.boxes_created.fetch_add(1, Ordering::Relaxed);
        }
        assert_eq!(metrics.num_running_boxes(), 5);

        // Stop 2 boxes
        storage.boxes_stopped.fetch_add(1, Ordering::Relaxed);
        storage.boxes_stopped.fetch_add(1, Ordering::Relaxed);
        assert_eq!(metrics.num_running_boxes(), 3);

        // 1 box fails to start
        storage.boxes_created.fetch_add(1, Ordering::Relaxed);
        storage.boxes_failed.fetch_add(1, Ordering::Relaxed);
        assert_eq!(metrics.num_running_boxes(), 3);

        // Stop remaining boxes
        for _ in 0..3 {
            storage.boxes_stopped.fetch_add(1, Ordering::Relaxed);
        }
        assert_eq!(metrics.num_running_boxes(), 0);
    }

    #[test]
    fn test_num_running_boxes_saturating_sub() {
        let storage = RuntimeMetricsStorage::new();
        let metrics = RuntimeMetrics::new(storage.clone());

        // Edge case: more stopped than created (shouldn't happen, but test safety)
        storage.boxes_created.fetch_add(1, Ordering::Relaxed);
        storage.boxes_stopped.fetch_add(5, Ordering::Relaxed);

        // Should saturate to 0, not underflow
        assert_eq!(metrics.num_running_boxes(), 0);
    }

    #[test]
    fn test_boxes_stopped_total() {
        let storage = RuntimeMetricsStorage::new();
        let metrics = RuntimeMetrics::new(storage.clone());

        assert_eq!(metrics.boxes_stopped_total(), 0);

        storage.boxes_stopped.fetch_add(3, Ordering::Relaxed);
        assert_eq!(metrics.boxes_stopped_total(), 3);
    }
}

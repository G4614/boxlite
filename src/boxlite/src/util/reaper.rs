//! Scoped shim-PID reaper (Issue #523).
//!
//! ## Why this exists
//!
//! `boxlite-shim` children whose Rust-side `Child` handle gets dropped
//! without anyone calling `wait()` on it become zombies when the shim
//! later exits. `std::process::Child`'s Drop impl is a no-op — it neither
//! kills nor waits — so any code path that holds a `Child`, then drops it
//! while the underlying process is still alive (or recently exited and
//! unreaped), leaks a zombie. Over time these accumulate; the
//! `CL84LvGx7RBE` incident showed 7+ `<defunct>` shims under the daemon.
//!
//! The load-bearing leak source in production is `ShimHandler` being
//! dropped without `ShimHandler::stop()` running to completion. The two
//! ways this happens routinely:
//!
//! - Box removal via `rt_impl::remove_box`, which SIGKILLs the shim by
//!   PID and never invokes `handler.stop()` — so the `Child` inside the
//!   `ShimHandler` is dropped while/after the shim is dying, no `wait()`
//!   is ever called, zombie.
//! - Runtime drop without `shutdown()`: the active `BoxImpl`s drop, their
//!   `ShimHandler`s drop with `Child` still inside, zombie.
//!
//! Init-failure paths are NOT a zombie source: `CleanupGuard::drop` calls
//! `handler.stop()` which reaps via `Child::wait()`. Likewise the normal
//! user-driven `boxlite stop` path.
//!
//! ## Why this is scoped, not daemon-wide
//!
//! An earlier attempt (PR #520, reverted commit on the same PR) installed
//! a `waitpid(-1, WNOHANG)` reaper. That races every other call site that
//! owns a `Child` handle: if the reaper wins, the owner's `wait()` returns
//! `ECHILD` and the exit code is lost. To dodge the race without auditing
//! every `Child::wait()` in the workspace, this reaper only touches PIDs
//! that were explicitly registered via [`ShimReaper::register`]. The only
//! registrar today is `ShimHandler::from_spawned`
//! (`src/boxlite/src/vmm/controller/shim.rs`), so the reaper's blast
//! radius is exactly the shim PID set.
//!
//! For shim PIDs, the three `let _ = process.wait();` sites in shim.rs
//! discard their results, so `ECHILD` from a reaper-win is safe.
//!
//! ## Why registration is a one-way door (no auto-unregister on Drop)
//!
//! `register(pid)` returns nothing. There is intentionally no RAII handle
//! that would unregister on Drop. Earlier draft had one; it produced a
//! load-bearing miss: when `ShimHandler` field-order-drops, the
//! `keepalive` field drops *before* the handle, which closes the watchdog
//! pipe → shim begins graceful exit → handle drops → registry is purged
//! → shim actually exits ~100 ms later → no one waits → zombie. The
//! reaper had been told "stop watching" microseconds before the very
//! event it existed to catch.
//!
//! With no auto-unregister, the registry stays populated until the sweep
//! observes `waitpid(pid, WNOHANG)` returning either `Reaped` (we just
//! collected it) or `Vanished` (someone else collected it, or the PID is
//! gone). Both outcomes drop the PID from the registry. Worst-case
//! membership is one [`REAPER_TICK`] (250 ms) after exit. The registry
//! never grows unbounded under normal traffic.
//!
//! ## Observability and self-healing
//!
//! [`ShimReaper::stats`] returns a snapshot of registry size, reaped
//! count, vanished count, sweep count, and last-sweep time. These power
//! both the in-process `RuntimeMetrics` and the REST `/v1/metrics`
//! endpoint so operators can spot "reaper isn't running" by watching the
//! `sweeps_completed` counter stop incrementing.
//!
//! If the worker thread dies (panic on a poisoned mutex, etc.), the next
//! `register()` call detects this via [`ShimReaper::worker_alive`] and
//! transparently respawns the worker. A loud `tracing::error!` is
//! emitted whenever this happens; the alert is the load-bearing signal
//! that something pathological is going on inside the reaper.
//!
//! ## Why polling, not SIGCHLD
//!
//! SIGCHLD plumbing in async Rust (signal-hook / tokio::signal::unix) is
//! process-global and adds a race surface against the runtime's other
//! signal handlers. A 250 ms HashSet scan is cheaper than that complexity
//! buys back. Worst-case zombie lifetime is 250 ms; tests verify drain in
//! < 2 s by polling, never sleeping.
//!
//! ## Why a std thread, not a tokio task
//!
//! `RuntimeImpl::new` is sync and can be called outside of any tokio
//! runtime context. A `std::thread` worker has no such precondition. The
//! work is sync anyway (`waitpid` + sleep), so there's no benefit to a
//! tokio task here.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

/// External sink the reaper writes its counters to on every sweep.
///
/// Used to expose reaper health through `RuntimeMetrics` and the REST
/// `/v1/metrics` endpoint without making the `util` module depend on the
/// `metrics` module — `RuntimeImpl::new` constructs one of these by
/// cloning the relevant `Arc<AtomicU64>`s out of its
/// `RuntimeMetricsStorage` and hands it to `ShimReaper::spawn_with_sink`.
#[derive(Clone, Default)]
pub struct MetricsSink {
    /// Monotonic total Reaped outcomes.
    pub reaped_total: Arc<AtomicU64>,
    /// Monotonic total Vanished outcomes.
    pub vanished_total: Arc<AtomicU64>,
    /// Current registry size (gauge).
    pub registered_now: Arc<AtomicU64>,
}

/// Recover from a poisoned mutex by extracting the inner data anyway.
///
/// Standard Rust pattern: poison indicates a panic happened while holding
/// the lock. For our `HashSet<u32>` registry, no operation can leave the
/// set in a torn state (insert/remove are atomic at the std-lib level),
/// so it's safe to recover and proceed. Without this, a poisoned mutex
/// would propagate panics through every code path that touches the
/// registry — defeating the self-healing respawn in `register()`.
fn lock_or_recover<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// How often the worker sweeps the registry for exited PIDs.
///
/// Trade-off: shorter = quicker reaping + slightly more CPU on a HashSet
/// scan. 250 ms keeps the worst-case zombie lifetime well below 1 s and
/// lets the unit test confirm reap in < 2 s without flakiness.
const REAPER_TICK: Duration = Duration::from_millis(250);

/// Outcome of a single `waitpid(pid, WNOHANG)` probe. Used by the worker
/// to decide whether to drop the PID from the registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReapOutcome {
    /// `waitpid` returned > 0 — we just reaped this PID. Drop from registry.
    Reaped,
    /// `waitpid` returned -1 with ECHILD — another reaper got it, or it's
    /// no longer our child. Either way nothing more to do. Drop from registry.
    Vanished,
    /// `waitpid` returned 0 — still alive, leave in registry.
    StillAlive,
}

fn probe_pid(pid: u32) -> ReapOutcome {
    let mut status: i32 = 0;
    // SAFETY: waitpid is async-signal-safe and has no Rust-visible
    // preconditions beyond a valid status pointer.
    let result = unsafe { libc::waitpid(pid as i32, &mut status, libc::WNOHANG) };
    if result > 0 {
        ReapOutcome::Reaped
    } else if result < 0 {
        // ECHILD is the only error path that's interesting here — it means
        // someone else (the owner's Child::wait, or an unrelated reaper)
        // already collected this PID, or this process is no longer our
        // child. EINTR can't happen with WNOHANG. Treat both as "done".
        ReapOutcome::Vanished
    } else {
        ReapOutcome::StillAlive
    }
}

fn unix_ms_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Internal state shared with the worker thread.
struct Inner {
    /// PIDs currently registered for reaping. Mutated under the Condvar lock.
    registry: Mutex<HashSet<u32>>,
    /// Signals worker to exit at the next wake-up.
    shutdown: AtomicBool,
    /// Lets the worker wake immediately on shutdown without burning a full
    /// REAPER_TICK. Pairs with `shutdown` boolean.
    wake: Condvar,
    /// Companion mutex for `wake`. Always locked before the condvar wait;
    /// holds no data — separated from `registry` so the worker doesn't
    /// hold the registry lock during the timed wait.
    wake_lock: Mutex<()>,
    /// Monotonic count of `Reaped` outcomes since reaper start.
    reaped_total: AtomicU64,
    /// Monotonic count of `Vanished` outcomes since reaper start.
    vanished_total: AtomicU64,
    /// Monotonic count of completed sweep passes. Stops incrementing if
    /// the worker dies — operators alert on this going flat.
    sweeps_completed: AtomicU64,
    /// Unix-ms timestamp of the last completed sweep. -1 means "never".
    /// Same purpose as sweeps_completed; lets dashboards show "X seconds
    /// since last sweep" without subscribing to a counter delta.
    last_sweep_at_ms: AtomicI64,
    /// Test-only panic injection. When set, the worker panics on its
    /// next loop iteration. Used by `worker_respawns_after_death` to
    /// deterministically simulate worker death since the production
    /// poison-recovery path (`lock_or_recover`) makes the natural
    /// "panic via mutex poison" scenario impossible to provoke.
    #[cfg(test)]
    test_panic_next_iteration: AtomicBool,
    /// Optional external counters mirrored on each sweep so
    /// `RuntimeMetrics` and `/v1/metrics` can show reaper activity.
    sink: Option<MetricsSink>,
}

/// Snapshot of reaper health. Returned by [`ShimReaper::stats`]. Cheap
/// to construct (one mutex acquire + four atomic loads). Designed to be
/// pulled into [`crate::metrics::RuntimeMetrics`] at request time rather
/// than maintained by a separate observer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReaperStats {
    /// Current number of PIDs tracked.
    pub registered: usize,
    /// Total `Reaped` outcomes since reaper start.
    pub reaped_total: u64,
    /// Total `Vanished` outcomes since reaper start.
    pub vanished_total: u64,
    /// Total completed sweeps. If this is increasing over time, the
    /// worker is alive and pumping. If it goes flat while PIDs are
    /// registered, the worker has died.
    pub sweeps_completed: u64,
    /// Unix milliseconds at the end of the most recent sweep. `None`
    /// if no sweep has finished yet (worker just started).
    pub last_sweep_at_ms: Option<i64>,
    /// Whether the worker thread is currently alive. False means the
    /// next `register()` will respawn it.
    pub worker_alive: bool,
}

/// Scoped reaper for `boxlite-shim` PIDs.
///
/// Owns a worker thread that periodically calls `waitpid(pid, WNOHANG)` on
/// every registered PID. Registrations are added by
/// `ShimHandler::from_spawned` and never explicitly removed — the worker
/// removes a PID once it observes the process is reaped or gone.
pub struct ShimReaper {
    inner: Arc<Inner>,
    worker: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl ShimReaper {
    /// Construct a reaper without external metrics export.
    pub fn spawn() -> Arc<Self> {
        Self::spawn_with_sink(None)
    }

    /// Construct a reaper and start its worker thread. If `sink` is
    /// `Some`, the worker mirrors its counters into the sink on every
    /// sweep — used by `RuntimeImpl::new` to feed `RuntimeMetricsStorage`.
    pub fn spawn_with_sink(sink: Option<MetricsSink>) -> Arc<Self> {
        let inner = Arc::new(Inner {
            registry: Mutex::new(HashSet::new()),
            shutdown: AtomicBool::new(false),
            wake: Condvar::new(),
            wake_lock: Mutex::new(()),
            reaped_total: AtomicU64::new(0),
            vanished_total: AtomicU64::new(0),
            sweeps_completed: AtomicU64::new(0),
            last_sweep_at_ms: AtomicI64::new(-1),
            #[cfg(test)]
            test_panic_next_iteration: AtomicBool::new(false),
            sink,
        });
        let reaper = Arc::new(Self {
            inner: Arc::clone(&inner),
            worker: Mutex::new(None),
        });
        reaper.spawn_worker();
        reaper
    }

    /// (Re)spawn the worker thread. Called by `spawn` and by `register`
    /// when it detects the prior worker has died.
    fn spawn_worker(self: &Arc<Self>) {
        let inner_for_worker = Arc::clone(&self.inner);
        let worker = std::thread::Builder::new()
            .name("boxlite-shim-reaper".into())
            .spawn(move || worker_loop(inner_for_worker))
            .expect("spawn reaper worker thread");
        *lock_or_recover(&self.worker) = Some(worker);
    }

    /// Register a shim PID for reaping.
    ///
    /// If the worker has died since the last call (panic on poisoned
    /// mutex, unexpected kernel state, etc.), this transparently respawns
    /// it. A `tracing::error!` is emitted whenever a respawn happens so
    /// operators get a load-bearing alert that something pathological is
    /// going on.
    ///
    /// No handle is returned. The reaper's sweep is the authoritative
    /// cleanup: when `waitpid(pid, WNOHANG)` reports the PID as `Reaped`
    /// or `Vanished`, it leaves the registry. There is no caller-side
    /// "unregister" because the load-bearing zombie source is exactly
    /// the case where the caller has no orderly chance to unregister
    /// before the shim dies (see module doc, "no auto-unregister on Drop").
    pub fn register(self: &Arc<Self>, pid: u32) {
        if !self.worker_alive() && !self.inner.shutdown.load(Ordering::SeqCst) {
            tracing::error!(
                pid,
                "Shim reaper worker thread died unexpectedly; respawning. \
                 Investigate sweep panic or mutex poisoning."
            );
            self.spawn_worker();
        }
        let new_size = {
            let mut reg = lock_or_recover(&self.inner.registry);
            reg.insert(pid);
            reg.len() as u64
        };
        if let Some(sink) = &self.inner.sink {
            // Update the gauge eagerly on register so dashboards see
            // the bump before the next sweep — avoids "we registered
            // 1000 PIDs but the metric stays at 0 for 250 ms" surprise.
            sink.registered_now.store(new_size, Ordering::Relaxed);
        }
    }

    /// Snapshot reaper health. See [`ReaperStats`].
    pub fn stats(&self) -> ReaperStats {
        let registered = lock_or_recover(&self.inner.registry).len();
        let last_ms = self.inner.last_sweep_at_ms.load(Ordering::Relaxed);
        ReaperStats {
            registered,
            reaped_total: self.inner.reaped_total.load(Ordering::Relaxed),
            vanished_total: self.inner.vanished_total.load(Ordering::Relaxed),
            sweeps_completed: self.inner.sweeps_completed.load(Ordering::Relaxed),
            last_sweep_at_ms: if last_ms < 0 { None } else { Some(last_ms) },
            worker_alive: self.worker_alive(),
        }
    }

    /// Is the worker thread still running?
    ///
    /// Returns `false` if `shutdown()` has been called (the explicit
    /// termination case) or if the worker has died unexpectedly (panic,
    /// etc.). Distinguishing the two requires also checking
    /// [`stats`]`.sweeps_completed` against the shutdown flag.
    pub fn worker_alive(&self) -> bool {
        match lock_or_recover(&self.worker).as_ref() {
            Some(h) => !h.is_finished(),
            None => false,
        }
    }

    /// Snapshot of currently registered PIDs. Test/debug aid; production
    /// callers should not need this.
    #[cfg(any(test, debug_assertions))]
    pub fn registered(&self) -> Vec<u32> {
        let mut v: Vec<u32> = lock_or_recover(&self.inner.registry)
            .iter()
            .copied()
            .collect();
        v.sort_unstable();
        v
    }

    /// Stop the worker and wait for it to exit. Idempotent — repeated calls
    /// after the first return immediately. Called from
    /// `RuntimeImpl::shutdown` so the worker doesn't outlive the runtime
    /// (especially important in test binaries that construct many runtimes
    /// serially).
    ///
    /// Sync (not async) because the worker is a `std::thread` and its
    /// `JoinHandle::join` is blocking. Worst-case wait is one Condvar
    /// wake-up + one sweep — typically sub-millisecond.
    pub fn shutdown(&self) {
        self.inner.shutdown.store(true, Ordering::SeqCst);
        // Wake the worker immediately so it doesn't sleep through REAPER_TICK.
        let _g = lock_or_recover(&self.inner.wake_lock);
        self.inner.wake.notify_all();
        drop(_g);
        let handle = lock_or_recover(&self.worker).take();
        if let Some(h) = handle {
            // best-effort: if the thread panicked, don't propagate.
            let _ = h.join();
        }
    }
}

impl Drop for ShimReaper {
    fn drop(&mut self) {
        // If shutdown() wasn't called explicitly, do it now so the worker
        // thread doesn't outlive its Inner. Safe to call from a sync Drop.
        self.shutdown();
    }
}

fn worker_loop(inner: Arc<Inner>) {
    loop {
        #[cfg(test)]
        if inner
            .test_panic_next_iteration
            .swap(false, Ordering::SeqCst)
        {
            panic!("worker: test_panic_next_iteration triggered");
        }
        if inner.shutdown.load(Ordering::SeqCst) {
            // One final drain pass on shutdown so we don't leave the
            // kernel holding zombies for any registered PIDs that already
            // exited.
            sweep(&inner);
            return;
        }
        sweep(&inner);
        // Wait up to REAPER_TICK or until shutdown wakes us. Holds the
        // empty `wake_lock` for the duration of the wait — not the
        // registry lock — so register stays responsive.
        let guard = lock_or_recover(&inner.wake_lock);
        let _ = inner.wake.wait_timeout(guard, REAPER_TICK);
    }
}

/// One pass over the registry. Drops any PID we successfully reaped or
/// that has vanished out from under us; leaves still-alive PIDs in place.
fn sweep(inner: &Inner) {
    // Snapshot the PID set so we don't hold the lock across waitpid().
    // waitpid(WNOHANG) is fast, but register also holds this mutex
    // briefly — short critical section is the right tradeoff.
    let snapshot: Vec<u32> = lock_or_recover(&inner.registry).iter().copied().collect();
    let mut to_remove: Vec<u32> = Vec::new();
    let mut reaped = 0u64;
    let mut vanished = 0u64;
    for pid in snapshot {
        match probe_pid(pid) {
            ReapOutcome::Reaped => {
                tracing::debug!(pid, "Scoped reaper collected shim exit");
                to_remove.push(pid);
                reaped += 1;
            }
            ReapOutcome::Vanished => {
                tracing::trace!(pid, "Scoped reaper: PID no longer reapable (ECHILD)");
                to_remove.push(pid);
                vanished += 1;
            }
            ReapOutcome::StillAlive => {}
        }
    }
    if !to_remove.is_empty() {
        let mut reg = lock_or_recover(&inner.registry);
        for pid in to_remove {
            reg.remove(&pid);
        }
    }
    if reaped > 0 {
        inner.reaped_total.fetch_add(reaped, Ordering::Relaxed);
    }
    if vanished > 0 {
        inner.vanished_total.fetch_add(vanished, Ordering::Relaxed);
    }
    inner.sweeps_completed.fetch_add(1, Ordering::Relaxed);
    inner
        .last_sweep_at_ms
        .store(unix_ms_now(), Ordering::Relaxed);

    // Mirror to the external sink for RuntimeMetrics / REST consumers.
    // The sink's reaped/vanished are monotonic counters; bump by the
    // deltas observed in this sweep. registered_now is a gauge; set
    // to the post-sweep registry size.
    if let Some(sink) = &inner.sink {
        if reaped > 0 {
            sink.reaped_total.fetch_add(reaped, Ordering::Relaxed);
        }
        if vanished > 0 {
            sink.vanished_total.fetch_add(vanished, Ordering::Relaxed);
        }
        let size = lock_or_recover(&inner.registry).len() as u64;
        sink.registered_now.store(size, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// Spawn a real subprocess that exits immediately and assert the
    /// reaper drops its PID from the registry within the worst-case
    /// REAPER_TICK budget. Uses `/bin/sh -c true` rather than a Rust
    /// in-process fork so the test exercises the actual waitpid path,
    /// not a stubbed liveness probe.
    #[test]
    fn reaps_exited_pid_within_one_second() {
        let reaper = ShimReaper::spawn();

        // Spawn `true` — exits with code 0 essentially instantly.
        let child = std::process::Command::new("/bin/sh")
            .args(["-c", "true"])
            .spawn()
            .expect("spawn /bin/sh -c true");
        let pid = child.id();
        // Deliberately leak the Child so its Drop doesn't wait() and
        // race the reaper — this mirrors the production "Child dropped
        // without wait" path the reaper exists to catch.
        std::mem::forget(child);

        reaper.register(pid);

        // Poll the registry. Must succeed within 1 s; if it doesn't,
        // the worker isn't reaping at the expected cadence.
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let still_registered = reaper.registered().contains(&pid);
            if !still_registered {
                break; // success
            }
            assert!(
                Instant::now() < deadline,
                "registry still contains {pid} after 1s — reaper didn't run"
            );
            std::thread::sleep(Duration::from_millis(50));
        }

        // The reaped counter must reflect this collection. (The integration
        // test asserts the full set-cardinality invariant; this one pins
        // the per-counter increment for a single PID.)
        assert!(reaper.stats().reaped_total >= 1);
        reaper.shutdown();
    }

    /// Pins the load-bearing post-fix invariant: registration is a one-way
    /// door, only the reaper's `waitpid` sweep removes a PID. This is the
    /// property the earlier RAII-handle design got wrong (registry was
    /// purged on owner Drop before the shim had finished exiting, missing
    /// the very zombie the reaper existed to catch).
    ///
    /// Concretely: register a long-lived child PID, give the worker
    /// plenty of time to sweep, and assert the PID is still registered.
    /// Then kill the child and assert the next sweep drains it.
    #[test]
    fn live_pid_stays_registered_until_actual_exit() {
        let reaper = ShimReaper::spawn();

        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "sleep 30"])
            .spawn()
            .expect("spawn sleep child");
        let pid = child.id();
        reaper.register(pid);

        // Three ticks worth — the worker has had plenty of chances to
        // run waitpid and observe "still alive".
        std::thread::sleep(REAPER_TICK * 3);
        assert!(
            reaper.registered().contains(&pid),
            "reaper must not unregister a still-running PID"
        );

        // Kill the child and wait via the Child handle ourselves so the
        // reaper's next sweep sees Vanished (ECHILD) rather than Reaped.
        child.kill().expect("kill child");
        child.wait().expect("wait child");

        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if !reaper.registered().contains(&pid) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "reaper didn't notice the PID vanished within 1 s"
            );
            std::thread::sleep(Duration::from_millis(50));
        }

        assert!(reaper.stats().vanished_total >= 1);
        reaper.shutdown();
    }

    /// Shutdown must terminate the worker thread quickly. The Condvar
    /// notify in `shutdown()` wakes the worker without it sleeping through
    /// a full REAPER_TICK — so `shutdown()` itself returns in well under
    /// the tick duration.
    #[test]
    fn shutdown_returns_promptly() {
        let reaper = ShimReaper::spawn();

        // Let one tick elapse so we know the worker is in steady-state
        // wait (not racing initial registration).
        std::thread::sleep(Duration::from_millis(50));

        let t0 = Instant::now();
        reaper.shutdown();
        let elapsed = t0.elapsed();
        assert!(
            elapsed < REAPER_TICK,
            "shutdown took {elapsed:?}, expected well under {REAPER_TICK:?}"
        );

        // Second shutdown is a no-op.
        let t0 = Instant::now();
        reaper.shutdown();
        assert!(
            t0.elapsed() < Duration::from_millis(10),
            "second shutdown should be near-instant"
        );

        // After shutdown, worker_alive() must report false.
        assert!(!reaper.worker_alive());
    }

    /// Burst-register many PIDs that all return ECHILD on waitpid (we're
    /// not the parent of any of them — they're a high-range u32 that the
    /// kernel hasn't issued). Asserts the sweep drains the whole batch
    /// without deadlocking on the registry mutex and within a reasonable
    /// time bound. The PID values are bounded to the i32 positive range
    /// so the libc::waitpid signature doesn't choke.
    #[test]
    fn registry_under_high_register_pressure() {
        const N: usize = 1024;
        const PRESSURE_BUDGET: Duration = Duration::from_secs(3);

        let reaper = ShimReaper::spawn();

        // Use a high range close to (but below) i32::MAX so we get
        // valid waitpid args that the kernel will reject with ECHILD.
        // Start at i32::MAX - 100_000 to avoid colliding with any
        // real-but-recycled PIDs.
        let base: u32 = (i32::MAX as u32) - 100_000;
        for i in 0..N {
            reaper.register(base + i as u32);
        }
        // Note: we don't assert `registered == N` immediately — by the
        // time the last few inserts complete, the worker may have
        // already swept the first few. The load-bearing assertion is
        // the drain below + the vanished_total counter.

        // All N should drain to Vanished within the budget.
        let deadline = Instant::now() + PRESSURE_BUDGET;
        loop {
            let remaining = reaper.stats().registered;
            if remaining == 0 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "registry still has {remaining} entries after {PRESSURE_BUDGET:?}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }

        let stats = reaper.stats();
        assert!(
            stats.vanished_total >= N as u64,
            "expected at least {N} vanished, got {}",
            stats.vanished_total
        );
        assert!(stats.sweeps_completed >= 1);

        reaper.shutdown();
    }

    /// Use the test-only `test_panic_next_iteration` flag to deterministic-
    /// ally crash the worker thread, then call register() and verify the
    /// reaper transparently respawns it. Pins the "self-healing under
    /// worker death" contract — a single reaper panic must not silently
    /// degrade the daemon into the leak-everything state.
    ///
    /// We can't reproduce this with mutex poisoning anymore: the
    /// production `lock_or_recover` path absorbs poison silently (which
    /// is the correct behavior for our registry — no torn-state risk
    /// with HashSet inserts). So the test uses an explicit injection
    /// flag instead.
    #[test]
    fn worker_respawns_after_death() {
        let reaper = ShimReaper::spawn();
        assert!(reaper.worker_alive());

        // Arm the panic flag and wake the worker so it hits the panic
        // on its very next iteration (not after a 250 ms wait).
        reaper
            .inner
            .test_panic_next_iteration
            .store(true, Ordering::SeqCst);
        let _g = reaper.inner.wake_lock.lock().unwrap();
        reaper.inner.wake.notify_all();
        drop(_g);

        // Wait until the worker thread is observably finished.
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if !reaper.worker_alive() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "worker didn't die within 2 s of injected panic"
            );
            std::thread::sleep(Duration::from_millis(50));
        }

        // register() should detect the dead worker and respawn it.
        reaper.register(42);
        assert!(
            reaper.worker_alive(),
            "register() must respawn the worker after observed death"
        );

        // The respawned worker must continue to function — its
        // sweeps_completed counter increments past the snapshot we
        // take here.
        let pre = reaper.stats().sweeps_completed;
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if reaper.stats().sweeps_completed > pre {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "respawned worker isn't pumping sweeps"
            );
            std::thread::sleep(Duration::from_millis(50));
        }

        reaper.shutdown();
    }

    /// Stats counters move under real activity. Independent assertion from
    /// the individual reap/vanish tests — verifies the aggregate snapshot
    /// API works (used by `RuntimeMetrics` and the REST `/v1/metrics`
    /// handler).
    #[test]
    fn stats_track_sweep_progress() {
        let reaper = ShimReaper::spawn();

        // Register a PID that will be Vanished (ECHILD) immediately.
        reaper.register((i32::MAX as u32) - 1);

        // Give worker at least two ticks.
        std::thread::sleep(REAPER_TICK * 2 + Duration::from_millis(50));

        let stats = reaper.stats();
        assert!(stats.worker_alive);
        assert!(
            stats.sweeps_completed >= 1,
            "sweeps_completed = {}",
            stats.sweeps_completed
        );
        assert!(stats.last_sweep_at_ms.is_some());
        assert!(
            stats.vanished_total >= 1,
            "vanished_total = {}",
            stats.vanished_total
        );

        reaper.shutdown();
    }
}

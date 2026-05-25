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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

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
    /// Construct a reaper and start its worker thread.
    pub fn spawn() -> Arc<Self> {
        let inner = Arc::new(Inner {
            registry: Mutex::new(HashSet::new()),
            shutdown: AtomicBool::new(false),
            wake: Condvar::new(),
            wake_lock: Mutex::new(()),
        });
        let inner_for_worker = Arc::clone(&inner);
        let worker = std::thread::Builder::new()
            .name("boxlite-shim-reaper".into())
            .spawn(move || worker_loop(inner_for_worker))
            .expect("spawn reaper worker thread");
        Arc::new(Self {
            inner,
            worker: Mutex::new(Some(worker)),
        })
    }

    /// Register a shim PID for reaping.
    ///
    /// No handle is returned. The reaper's sweep is the authoritative
    /// cleanup: when `waitpid(pid, WNOHANG)` reports the PID as `Reaped`
    /// or `Vanished`, it leaves the registry. There is no caller-side
    /// "unregister" because the load-bearing zombie source is exactly
    /// the case where the caller has no orderly chance to unregister
    /// before the shim dies (see module doc, "no auto-unregister on Drop").
    pub fn register(&self, pid: u32) {
        self.inner.registry.lock().unwrap().insert(pid);
    }

    /// Snapshot of currently registered PIDs. Test/debug aid; production
    /// callers should not need this.
    #[cfg(any(test, debug_assertions))]
    pub fn registered(&self) -> Vec<u32> {
        let mut v: Vec<u32> = self.inner.registry.lock().unwrap().iter().copied().collect();
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
        let _g = self.inner.wake_lock.lock().unwrap();
        self.inner.wake.notify_all();
        drop(_g);
        let handle = self.worker.lock().unwrap().take();
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
        if inner.shutdown.load(Ordering::SeqCst) {
            // One final drain pass on shutdown so we don't leave the
            // kernel holding zombies for any registered PIDs that already
            // exited.
            sweep(&inner.registry);
            return;
        }
        sweep(&inner.registry);
        // Wait up to REAPER_TICK or until shutdown wakes us. Holds the
        // empty `wake_lock` for the duration of the wait — not the
        // registry lock — so register stays responsive.
        let guard = inner.wake_lock.lock().unwrap();
        let (_g, _timed_out) = inner
            .wake
            .wait_timeout(guard, REAPER_TICK)
            .expect("Condvar poisoned");
    }
}

/// One pass over the registry. Drops any PID we successfully reaped or
/// that has vanished out from under us; leaves still-alive PIDs in place.
fn sweep(registry: &Mutex<HashSet<u32>>) {
    // Snapshot the PID set so we don't hold the lock across waitpid().
    // waitpid(WNOHANG) is fast, but register also holds this mutex
    // briefly — short critical section is the right tradeoff.
    let snapshot: Vec<u32> = registry.lock().unwrap().iter().copied().collect();
    let mut to_remove: Vec<u32> = Vec::new();
    for pid in snapshot {
        match probe_pid(pid) {
            ReapOutcome::Reaped => {
                tracing::debug!(pid, "Scoped reaper collected shim exit");
                to_remove.push(pid);
            }
            ReapOutcome::Vanished => {
                tracing::trace!(pid, "Scoped reaper: PID no longer reapable (ECHILD)");
                to_remove.push(pid);
            }
            ReapOutcome::StillAlive => {}
        }
    }
    if !to_remove.is_empty() {
        let mut reg = registry.lock().unwrap();
        for pid in to_remove {
            reg.remove(&pid);
        }
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
        // This proves the registry drains on ECHILD just as it drains on
        // Reaped — the property that lets `ShimHandler::stop` reap via
        // its own `Child::wait()` without coordinating with the reaper.
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
    }
}

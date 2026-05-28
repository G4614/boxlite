//! Integration test for the scoped shim-PID reaper (Issue #523).
//!
//! Verifies the acceptance criterion: "N init failures leave 0 zombies".
//! Simulates that scenario by spawning N short-lived subprocesses that all
//! exit immediately, registering each with the reaper, and asserting the
//! reaper drains its registry — and that no `<defunct>` entries remain
//! visible in `/proc` — within the < 2 s test budget.
//!
//! Uses `/bin/sh -c true` rather than `boxlite-shim` because the reaper is
//! agnostic to the binary; it only cares about the PID lifecycle. This
//! keeps the test fast and VM-free.

use boxlite::util::ShimReaper;
use std::process::Command;
use std::time::{Duration, Instant};

/// Simulates N abandoned mid-init shims. Each "failure" is a real fork +
/// exec that exits with code 0 essentially instantly; the test
/// `std::mem::forget`s the Child handle so its Drop doesn't `wait()` and
/// race the reaper — matching the production path where the abandoned
/// shim never gets a Rust-side `Child::wait()`.
#[test]
fn n_init_failures_leave_zero_zombies() {
    const N: usize = 8; // matches the CL84LvGx7RBE incident count (7+ zombies)
    const REAP_BUDGET: Duration = Duration::from_secs(2);

    let reaper = ShimReaper::spawn();

    // Spawn N "failed init" stand-ins and register each PID. No handle
    // is returned (and none is needed) — the reaper's sweep is the
    // authoritative cleanup, register-then-forget is the production
    // pattern.
    let mut pids = Vec::with_capacity(N);
    for _ in 0..N {
        let child = Command::new("/bin/sh")
            .args(["-c", "true"])
            .spawn()
            .expect("spawn /bin/sh -c true");
        let pid = child.id();
        // Mirror the abandoned-mid-init path: leak the Child so its Drop
        // doesn't reap the process out from under the reaper.
        std::mem::forget(child);
        reaper.register(pid);
        pids.push(pid);
    }

    // All N must be reaped within the budget. Poll, not sleep — finishing
    // earlier than REAP_BUDGET keeps the test fast on healthy machines.
    let deadline = Instant::now() + REAP_BUDGET;
    loop {
        let remaining = reaper.registered();
        if remaining.is_empty() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "reaper still tracking {} of {N} PIDs after {REAP_BUDGET:?}: {remaining:?}",
            remaining.len()
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // Belt-and-suspenders: no zombie should remain visible in /proc.
    // On Linux, a reaped or vanished process has no /proc/<pid> entry at
    // all; an unreaped zombie shows State: Z. Either way, the assertion
    // below catches the regression we care about.
    for pid in &pids {
        let status_path = format!("/proc/{pid}/status");
        match std::fs::read_to_string(&status_path) {
            Ok(content) => {
                assert!(
                    !content.contains("State:\tZ"),
                    "PID {pid} is still a zombie in /proc:\n{content}"
                );
            }
            Err(_) => {
                // No /proc entry: process is gone. Expected outcome.
            }
        }
    }

    reaper.shutdown();
}

/// Pins the post-fix invariant that `register` is idempotent: calling it
/// twice for the same PID does not double-track, and a re-registration
/// after the reaper has cleaned up a prior PID works exactly like a
/// first-time registration. Matches the production "retry" path where an
/// init attempt fails and a fresh spawn re-enters the same code paths.
#[test]
fn register_is_idempotent_and_survives_prior_cleanup() {
    let reaper = ShimReaper::spawn();

    // First registration of a same-PID stand-in: the OS will assign pid1
    // to the first child; that PID may even get re-used by the second
    // spawn (rare but valid). The contract this test pins is that double-
    // registration is a no-op and re-registration after sweep-cleanup is
    // a clean state.
    let child1 = Command::new("/bin/sh")
        .args(["-c", "true"])
        .spawn()
        .expect("spawn first sh");
    let pid1 = child1.id();
    std::mem::forget(child1);
    reaper.register(pid1);
    reaper.register(pid1); // idempotent — HashSet insert returns false but does not error

    assert_eq!(
        reaper.registered().iter().filter(|p| **p == pid1).count(),
        1,
        "double register must result in a single registry entry"
    );

    // Wait for reaper to drain pid1.
    let deadline = Instant::now() + Duration::from_secs(2);
    while reaper.registered().contains(&pid1) {
        assert!(Instant::now() < deadline, "pid1 never reaped");
        std::thread::sleep(Duration::from_millis(50));
    }

    // Register a second PID. Must work without state pollution from
    // the first.
    let child2 = Command::new("/bin/sh")
        .args(["-c", "true"])
        .spawn()
        .expect("spawn second sh");
    let pid2 = child2.id();
    std::mem::forget(child2);
    reaper.register(pid2);

    assert!(reaper.registered().contains(&pid2));
    let deadline = Instant::now() + Duration::from_secs(2);
    while reaper.registered().contains(&pid2) {
        assert!(Instant::now() < deadline, "pid2 never reaped");
        std::thread::sleep(Duration::from_millis(50));
    }

    reaper.shutdown();
}

/// Scoping guarantee: a child whose PID was never registered must be left
/// untouched, so its owner's `wait()` still returns the real exit code. This
/// is the property whose absence got PR #520's global `waitpid(-1)` reaper
/// reverted (Issue #523) — it consumed children it did not own, and their
/// owners then got `ECHILD` with the exit code lost. Guards against
/// re-widening the reaper's blast radius past the registered shim-PID set.
#[test]
fn unregistered_child_exit_code_is_not_stolen() {
    let reaper = ShimReaper::spawn();

    // A real child that exits with a DISTINCT code, deliberately NOT
    // registered with the reaper.
    let mut child = Command::new("/bin/sh")
        .args(["-c", "exit 42"])
        .spawn()
        .expect("spawn /bin/sh -c 'exit 42'");

    // ~3x the reaper's 250 ms tick: ample cycles for a scoped reaper to sweep
    // its (empty) registry without touching this child. A global waitpid(-1)
    // reaper would consume it here, and the owner's wait() below would get
    // ECHILD instead of exit code 42.
    std::thread::sleep(Duration::from_millis(800));

    let status = child
        .wait()
        .expect("owner must still be able to wait its own child");
    assert_eq!(
        status.code(),
        Some(42),
        "unregistered child's exit code must survive — reaper must stay scoped, not waitpid(-1)"
    );

    reaper.shutdown();
}

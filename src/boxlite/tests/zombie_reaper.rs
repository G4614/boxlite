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

    // Spawn N "failed init" stand-ins and register each PID.
    let mut handles = Vec::with_capacity(N);
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
        handles.push(reaper.register(pid));
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

    // Dropping `handles` here is a no-op for unregistration (the PIDs
    // already left the registry via the reap path), but it documents
    // the ownership relationship.
    drop(handles);
    reaper.shutdown();
}

/// Re-register a PID after Drop unregistered it. Encodes the "retry"
/// path: an init pipeline that fails, gets cleaned up, and tries again
/// from scratch — the second attempt's shim PID must register cleanly
/// even though the first attempt's PID went through the same registry.
#[test]
fn registry_accepts_re_registration_after_drop() {
    let reaper = ShimReaper::spawn();

    let child = Command::new("/bin/sh")
        .args(["-c", "true"])
        .spawn()
        .expect("spawn first sh");
    let pid1 = child.id();
    std::mem::forget(child);
    let h1 = reaper.register(pid1);

    // Wait for reap, then drop the (now-stale) handle.
    let deadline = Instant::now() + Duration::from_secs(2);
    while reaper.registered().contains(&pid1) {
        assert!(Instant::now() < deadline, "first pid never reaped");
        std::thread::sleep(Duration::from_millis(50));
    }
    drop(h1);

    // Now register a second PID. Must work without state pollution
    // from the first.
    let child2 = Command::new("/bin/sh")
        .args(["-c", "true"])
        .spawn()
        .expect("spawn second sh");
    let pid2 = child2.id();
    std::mem::forget(child2);
    let _h2 = reaper.register(pid2);

    assert!(reaper.registered().contains(&pid2));
    let deadline = Instant::now() + Duration::from_secs(2);
    while reaper.registered().contains(&pid2) {
        assert!(Instant::now() < deadline, "second pid never reaped");
        std::thread::sleep(Duration::from_millis(50));
    }

    reaper.shutdown();
}

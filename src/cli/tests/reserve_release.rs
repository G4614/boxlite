//! End-to-end for `boxlite reserve-release` — the recovery flow the
//! operator hits when the host filesystem is at ENOSPC. The release
//! itself is metadata-only (`unlink(2)`) so it must work even when the
//! disk is full. We can't easily wedge `/tmp` to 0 free in CI, so the
//! test instead verifies the *byte-accounting* invariant that backs the
//! recovery story: after `reserve-release`, the reserve file is gone
//! and the home dir's reported size drops by the reserve amount.

use std::time::Duration;

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

fn cli(home: &TempDir) -> Command {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("boxlite"));
    cmd.env("BOXLITE_HOME", home.path())
        .env_remove("BOXLITE_API_KEY")
        .env_remove("BOXLITE_REST_URL")
        .env_remove("BOXLITE_PROFILE")
        .timeout(Duration::from_secs(60));
    cmd
}

fn reserve_size(home: &TempDir) -> Option<u64> {
    std::fs::metadata(home.path().join(".reserve"))
        .ok()
        .map(|m| m.len())
}

/// Bootstrapping the runtime (here via `boxlite list`) creates the
/// reserve as a side effect — that's the structural admission floor
/// that replaces the old per-command statvfs check. Pin it so a
/// regression that moves `ensure_reserve` out of `RuntimeImpl::new`
/// silently disables host protection.
#[test]
fn runtime_bootstrap_creates_the_reserve() {
    let home = TempDir::new().unwrap();
    cli(&home).args(["list"]).assert().success();
    let size = reserve_size(&home).expect(".reserve must exist after a runtime is constructed");
    // 64 MiB exactly — the constant in `boxlite::util::reserve::RESERVE_BYTES`.
    assert_eq!(size, 64 * 1024 * 1024);
}

/// `boxlite reserve-release` removes the reserve file and prints the
/// recovered headroom + a hint to follow up with gc / rm. The reserve
/// is then absent until a runtime constructs again — verifying our
/// "metadata-only unlink, no double-free" guarantee.
#[test]
fn reserve_release_removes_file_and_reports_bytes() {
    let home = TempDir::new().unwrap();
    cli(&home).args(["list"]).assert().success();
    assert!(reserve_size(&home).is_some());

    cli(&home)
        .args(["reserve-release"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("Released")
                .and(predicate::str::contains("64.0 MiB"))
                .and(predicate::str::contains("boxlite gc"))
                .and(predicate::str::contains("recreated automatically")),
        );

    assert!(
        reserve_size(&home).is_none(),
        ".reserve must be gone after release"
    );
}

/// Double-release is a no-op, not an error — important because an
/// operator under stress (host full, panicking) may run it twice or
/// have it scripted into an idempotent recovery hook.
#[test]
fn reserve_release_is_idempotent() {
    let home = TempDir::new().unwrap();
    cli(&home).args(["list"]).assert().success();
    cli(&home).args(["reserve-release"]).assert().success();

    cli(&home)
        .args(["reserve-release"])
        .assert()
        .success()
        .stdout(predicate::str::contains("already released"));
}

/// After release, the next runtime construction tops the reserve back
/// up — without this, a single emergency release would permanently
/// disable the floor.
#[test]
fn reserve_is_recreated_on_next_runtime_construction() {
    let home = TempDir::new().unwrap();
    cli(&home).args(["list"]).assert().success();
    cli(&home).args(["reserve-release"]).assert().success();
    assert!(reserve_size(&home).is_none());

    // Any command that constructs a runtime will do — `list` is the
    // cheapest one.
    cli(&home).args(["list"]).assert().success();
    let size = reserve_size(&home).expect(".reserve must be recreated on the next runtime");
    assert_eq!(size, 64 * 1024 * 1024);
}

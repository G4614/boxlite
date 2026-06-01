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

/// **The user-visible recovery story end-to-end.**
///
/// The four tests above pin the *mechanism* (reserve created / removed /
/// recreated / file gone). This pins the *outcome* the reserve exists for:
///
///   1. Bootstrap boxlite home — reserve laid down
///   2. Fill the host fs with garbage until any further write returns
///      ENOSPC (simulates "operator's disk is full from any cause")
///   3. Verify a fresh write probe really does ENOSPC
///   4. Run `boxlite reserve-release`
///   5. Verify the same write probe now succeeds — i.e. the freed 64 MiB
///      is *actually usable*, not just an accounting artifact
///
/// Without this, a regression that switched `release` to `truncate(0)`,
/// forgot the fsync the host fs needs to reclaim, or accidentally
/// re-fallocated the reserve before returning, would pass every other
/// test but leave the operator stuck after recovery.
///
/// Gated on `BOXLITE_RESERVE_TEST_HOME` because the test fills the
/// filesystem to ENOSPC — only safe on a dedicated small mount. A
/// loopback ext4 works well:
///
/// ```sh
/// dd if=/dev/zero of=/tmp/boxlite-reserve-test.img bs=1M count=256
/// mkfs.ext4 -F /tmp/boxlite-reserve-test.img
/// sudo mkdir -p /mnt/boxlite-reserve-test
/// sudo mount -o loop /tmp/boxlite-reserve-test.img /mnt/boxlite-reserve-test
/// sudo chown $USER /mnt/boxlite-reserve-test
/// export BOXLITE_RESERVE_TEST_HOME=/mnt/boxlite-reserve-test
/// cargo test -p boxlite-cli --test reserve_release release_unblocks_writes
/// ```
///
/// On normal CI without the mount, the test prints a skip notice and
/// returns success — same posture as the original PR's
/// `real_disk_pressure_crosses_to_reject`.
#[test]
fn release_unblocks_writes_on_a_full_host() {
    let Ok(home_str) = std::env::var("BOXLITE_RESERVE_TEST_HOME") else {
        eprintln!(
            "skipping release_unblocks_writes_on_a_full_host: \
             set BOXLITE_RESERVE_TEST_HOME=/path/to/dedicated-small-mount \
             (e.g. a 256 MiB loop-mounted ext4) to opt in"
        );
        return;
    };
    let home_path = std::path::Path::new(&home_str);

    // Clean slate. Don't `remove_dir_all` the mount root itself — just
    // its contents — so an operator who scripted the loop mount doesn't
    // suddenly find the dir gone.
    for entry in std::fs::read_dir(home_path)
        .expect("BOXLITE_RESERVE_TEST_HOME must be a readable existing dir")
        .flatten()
    {
        let _ =
            std::fs::remove_file(entry.path()).or_else(|_| std::fs::remove_dir_all(entry.path()));
    }

    // 1. Bootstrap — `boxlite list` runs RuntimeImpl::new, which lays
    //    down the 64 MiB reserve.
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("boxlite"));
    cmd.env("BOXLITE_HOME", home_path)
        .env_remove("BOXLITE_API_KEY")
        .env_remove("BOXLITE_REST_URL")
        .env_remove("BOXLITE_PROFILE")
        .timeout(std::time::Duration::from_secs(60));
    cmd.args(["list"]).assert().success();
    assert_eq!(
        std::fs::metadata(home_path.join(".reserve")).unwrap().len(),
        64 * 1024 * 1024,
        "reserve must be 64 MiB after bootstrap"
    );

    // 2. Fill the rest of the FS with one big garbage file. Sequential
    //    write rather than fallocate so the failure mode at ENOSPC is
    //    exactly the "kernel returned -ENOSPC mid-write" path a real
    //    workload hits.
    //
    //    Drain in progressively smaller chunks so the file system ends
    //    truly stuck — without the small-chunk passes, the 4 MiB write
    //    returns ENOSPC while there are still 3.99 MiB free, and a
    //    later 1-byte probe would slip through and falsify step 3.
    let garbage = home_path.join("hostfill.bin");
    {
        use std::fs::File;
        use std::io::Write;
        let mut f = File::create(&garbage).expect("create garbage file");
        for chunk_size in [4 * 1024 * 1024, 64 * 1024, 4 * 1024, 1] {
            let buf = vec![0u8; chunk_size];
            loop {
                match f.write_all(&buf) {
                    Ok(()) => {}
                    Err(e) if e.raw_os_error() == Some(28) => break,
                    Err(e) => panic!("unexpected fill error: {e}"),
                }
            }
        }
        let _ = f.sync_all();
    }

    // 3. Sanity: an unprivileged write must now fail with ENOSPC. The
    //    reserve sits there occupying 64 MiB; the rest of the fs is
    //    pinned by `hostfill.bin`.
    let probe_path = home_path.join("write-probe.bin");
    let probe_err = std::fs::write(&probe_path, b"x")
        .expect_err("write must ENOSPC before release; the test fixture is wrong");
    assert_eq!(
        probe_err.raw_os_error(),
        Some(28),
        "the pre-release write must specifically be ENOSPC; got: {probe_err:?}"
    );

    // 4. Run the recovery command. `unlink(2)` is metadata-only, so it
    //    must succeed even at zero unprivileged free.
    let mut release = Command::new(assert_cmd::cargo::cargo_bin!("boxlite"));
    release
        .env("BOXLITE_HOME", home_path)
        .env_remove("BOXLITE_API_KEY")
        .env_remove("BOXLITE_REST_URL")
        .env_remove("BOXLITE_PROFILE")
        .timeout(std::time::Duration::from_secs(30));
    release
        .args(["reserve-release"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Released"));

    // 5. The actual invariant the user cares about: with the reserve
    //    released, an unprivileged write now succeeds. Without this
    //    behaving, `boxlite rm` / `boxlite gc` would still fail and
    //    the whole recovery affordance would be a placebo.
    std::fs::write(&probe_path, b"x").expect(
        "write after reserve-release must succeed — the freed 64 MiB \
         is what `rm` and `gc` need to make progress on a full host",
    );

    // Cleanup so a CI loop can re-run the test.
    let _ = std::fs::remove_file(&probe_path);
    let _ = std::fs::remove_file(&garbage);
}

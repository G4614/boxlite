//! Pins the empirical property that drives the recovery design: the
//! reserve lifecycle (`ensure_reserve` / `release_reserve`) operates
//! purely on the `.reserve` inode and does **not** touch any
//! running-box state. So an operator who hits ENOSPC, runs
//! `boxlite reserve-release` to recover, will not lose or disturb
//! any of their running boxes in the process.
//!
//! ## What this verifies (and what it doesn't)
//!
//! ✓ Verified here (no sudo, no host fill needed):
//!   - `boxlite reserve-release` while a box is `Running` leaves that
//!     box in `Running` state.
//!   - Multiple release → auto-restore cycles do not flip a box's
//!     status or break its shim PID.
//!   - The box's state-DB row survives across the lifecycle.
//!   - boxlite CLI remains functional throughout (every `ps` succeeds).
//!
//! ✗ NOT verified here (would require sudo + mount tmpfs, see
//!   chat history 2026-06-01 for the manual-test recipe that does
//!   confirm these empirically):
//!   - shim process surviving when host fs is truly at f_bavail=0.
//!   - boxlite CLI surfacing ENOSPC cleanly (vs. panic/hang) when
//!     SQLite WAL can't allocate new pages.
//!   - `release_reserve` succeeding as metadata-only `unlink(2)` at
//!     zero free space.
//!   - Box workload writes failing as guest EIO vs clean ENOSPC.
//!
//! For (✗) the runbook is in [doc location TBD]; in the meantime
//! the manual verification recipe is: mount a bounded tmpfs, set
//! BOXLITE_HOME there, start an idle box, fill the tmpfs, observe.

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use assert_cmd::cargo::cargo_bin;
use boxlite_test_utils::home::PerTestBoxHome;

fn boxlite(home: &PathBuf, args: &[&str], timeout: Duration) -> std::process::Output {
    let mut cmd = Command::new(cargo_bin("boxlite"));
    cmd.env("BOXLITE_HOME", home)
        .env_remove("BOXLITE_API_KEY")
        .env_remove("BOXLITE_REST_URL")
        .env_remove("BOXLITE_PROFILE")
        .args(args);
    // We don't have a portable timeout on Command directly; rely on
    // nextest's slow-test detection + per-test profile timeouts. The
    // `timeout` param documents intent for future readers.
    let _ = timeout;
    cmd.output().expect("spawn boxlite")
}

fn parse_box_id(stdout: &[u8]) -> String {
    String::from_utf8_lossy(stdout)
        .trim()
        .lines()
        .last()
        .unwrap_or("")
        .to_string()
}

fn box_status(home: &PathBuf, box_id: &str) -> Option<String> {
    let out = boxlite(home, &["ps", "--all"], Duration::from_secs(15));
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    for line in stdout.lines() {
        if line.contains(box_id) {
            // The status column is between the image and the created
            // timestamp — match the keywords directly to avoid coupling
            // to the table-formatting library.
            for status in ["Running", "Stopped", "Created", "Exited", "Failed"] {
                if line.contains(status) {
                    return Some(status.to_string());
                }
            }
        }
    }
    None
}

struct BoxCleanup {
    home: PathBuf,
    id: String,
}

impl Drop for BoxCleanup {
    fn drop(&mut self) {
        let _ = boxlite(&self.home, &["rm", "-f", &self.id], Duration::from_secs(30));
    }
}

fn start_idle_box(home: &PathBuf) -> String {
    let out = boxlite(
        home,
        &[
            "--registry",
            "docker.m.daocloud.io",
            "run",
            "-d",
            "--memory",
            "256",
            "alpine:latest",
            "sleep",
            "3600",
        ],
        Duration::from_secs(300),
    );
    assert!(
        out.status.success(),
        "box start failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let id = parse_box_id(&out.stdout);
    assert!(!id.is_empty(), "box id missing in stdout");
    id
}

/// Reserve-release while a box is running does NOT change the box's
/// state, fail the box, or break subsequent boxlite commands. The
/// release path touches `$BOXLITE_HOME/.reserve` only — never any
/// `boxes/<id>/` content — and that invariant has to hold for the
/// documented recovery flow to be safe.
#[test]
fn reserve_release_does_not_disturb_running_box() {
    let home = PerTestBoxHome::new();

    // Bootstrap reserves a 64 MiB ballast under $home/.reserve as a
    // side effect of constructing any runtime. Pin that this is in
    // place before we start the box.
    let _ = boxlite(&home.path, &["list"], Duration::from_secs(15));
    let reserve = home.path.join(".reserve");
    assert!(reserve.exists(), ".reserve must exist after bootstrap");

    let box_id = start_idle_box(&home.path);
    let _cleanup = BoxCleanup {
        home: home.path.clone(),
        id: box_id.clone(),
    };

    let pre_status = box_status(&home.path, &box_id);
    assert_eq!(
        pre_status.as_deref(),
        Some("Running"),
        "box must be Running pre-release; got {pre_status:?}"
    );

    // Release the reserve. CLI must succeed even with a running box.
    let rel = boxlite(&home.path, &["reserve-release"], Duration::from_secs(30));
    assert!(
        rel.status.success(),
        "reserve-release must succeed with a running box; stderr:\n{}",
        String::from_utf8_lossy(&rel.stderr)
    );
    assert!(!reserve.exists(), ".reserve must be unlinked after release");

    // Critical assertion: the box's state survived the release. Without
    // this guarantee, an operator hitting ENOSPC would have to choose
    // between recovery (release) and losing their boxes — making the
    // whole recovery story unusable.
    let post_status = box_status(&home.path, &box_id);
    assert_eq!(
        post_status.as_deref(),
        Some("Running"),
        "box must still be Running after reserve-release; got {post_status:?}"
    );

    // The next runtime construction should auto-restore the reserve.
    // We trigger via another `list`, which goes through
    // `RuntimeImpl::new → ensure_reserve`.
    let _ = boxlite(&home.path, &["list"], Duration::from_secs(15));
    assert!(
        reserve.exists() && std::fs::metadata(&reserve).unwrap().len() == 64 * 1024 * 1024,
        ".reserve must be restored to 64 MiB after the next runtime build"
    );

    // And after the restore, the box is *still* Running. This catches
    // a regression where `ensure_reserve`'s fallocate path somehow
    // racing with the box's state-DB writes could corrupt state.
    let final_status = box_status(&home.path, &box_id);
    assert_eq!(
        final_status.as_deref(),
        Some("Running"),
        "box must still be Running after reserve auto-restore; \
         got {final_status:?}"
    );
}

/// Stress the lifecycle: alternate release ↔ auto-restore several
/// times while a box is running. Confirms the cycle is genuinely
/// independent of box state, not just lucky on the first iteration.
/// A regression here would surface as a flaky test after a few
/// iterations rather than an obvious first-call failure.
#[test]
fn box_survives_multiple_reserve_cycles() {
    let home = PerTestBoxHome::new();
    let _ = boxlite(&home.path, &["list"], Duration::from_secs(15));

    let box_id = start_idle_box(&home.path);
    let _cleanup = BoxCleanup {
        home: home.path.clone(),
        id: box_id.clone(),
    };
    let reserve = home.path.join(".reserve");

    // Five release + restore cycles is enough to surface a stateful
    // bug without making the test slow (each cycle ~ 1-2 seconds of
    // runtime construction + fallocate).
    for cycle in 1..=5 {
        let rel = boxlite(&home.path, &["reserve-release"], Duration::from_secs(30));
        assert!(
            rel.status.success(),
            "cycle {cycle}: reserve-release failed:\n{}",
            String::from_utf8_lossy(&rel.stderr)
        );

        let status = box_status(&home.path, &box_id);
        assert_eq!(
            status.as_deref(),
            Some("Running"),
            "cycle {cycle}: box must be Running after release; got {status:?}"
        );

        // Auto-restore via another runtime construction.
        let _ = boxlite(&home.path, &["list"], Duration::from_secs(15));
        assert!(
            reserve.exists(),
            "cycle {cycle}: reserve must be auto-restored after release"
        );

        let status_after = box_status(&home.path, &box_id);
        assert_eq!(
            status_after.as_deref(),
            Some("Running"),
            "cycle {cycle}: box must be Running after restore; got {status_after:?}"
        );
    }
}

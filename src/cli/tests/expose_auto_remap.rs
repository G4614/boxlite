//! Regression: when the desired host port for an image's EXPOSE entry
//! is already bound on the host, `boxlite run` (no explicit `-p`) MUST
//! auto-remap that EXPOSE entry to an OS-allocated ephemeral host port
//! instead of failing fast with `gvproxy_create failed`.
//!
//! The companion path (`gvproxy_port_conflict_fails_fast_with_named_error`
//! in `gvproxy_port_conflict.rs`) covers the *explicit* `-p HOST:GUEST`
//! case where the user picked the host port deliberately — that path
//! still fails fast. This test covers the orthogonal EXPOSE-only path:
//! the runtime owns host-port selection there, so a conflict must be
//! resolved silently by picking another port.
//!
//! Image choice: `redis:alpine` because (a) it ships with `EXPOSE 6379`
//! in its manifest so the auto-publish code path engages with no `-p`,
//! and (b) the image is tiny (~13 MB compressed). The test does NOT
//! require `--privileged` and runs in the standard
//! `make test:integration:cli` matrix.
//!
//! Two-side regression contract (per CLAUDE.md "Reproduce-before-fix"):
//!   - Pre-fix (helper returns `desired_port` unconditionally): box
//!     never reaches Running because `gvproxy_create` errors out with
//!     EADDRINUSE on port 6379 → `boxlite run -d` exits non-zero →
//!     this test fails at the rc check.
//!   - Post-fix (`resolve_expose_host_port` falls back to an ephemeral
//!     port): box reaches Running, `BoxState::port_mappings` records
//!     `host_port != 6379, source = auto_remap`, `boxlite inspect`
//!     surfaces both → this test passes.

mod common;

use serde_json::Value;
use std::net::TcpListener;
use std::time::Duration;

const EXPOSE_PORT: u16 = 6379;

#[test]
fn expose_auto_remap_falls_back_when_desired_port_busy() {
    // Pre-bind 0.0.0.0:6379 so the desired host port (= guest port 6379
    // from the redis image's EXPOSE) is unavailable. If another process
    // on the host already holds 6379, the runtime would auto-remap on
    // its own and we'd never exercise the conflict path — skip cleanly
    // instead of producing a flaky result.
    let blocker = match TcpListener::bind(("0.0.0.0", EXPOSE_PORT)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!(
                "SKIP expose_auto_remap_falls_back_when_desired_port_busy: \
                 cannot pre-bind 0.0.0.0:{EXPOSE_PORT} ({e}). The test needs \
                 an unbound EXPOSE port on the host to force the conflict \
                 path; rerun on a host that isn't already serving on \
                 {EXPOSE_PORT}."
            );
            return;
        }
    };
    // Hold the listener for the test's full duration (drop at scope end).

    let ctx = common::boxlite();

    // Run redis:alpine detached with no explicit `-p`. The image's
    // EXPOSE 6379 is the only thing that produces a host-side mapping,
    // and since 6379 is busy (we hold it above) the runtime MUST fall
    // back to an OS-allocated ephemeral host port.
    //
    // We don't depend on redis actually accepting traffic here — the
    // contract under test is "the box reaches Running and inspect
    // surfaces the auto-remap", not "the service is reachable" (that
    // belongs in a separate end-to-end test).
    let run_output = ctx
        .new_cmd()
        .timeout(Duration::from_secs(120))
        .args(["run", "-d", "redis:alpine"])
        .output()
        .expect("spawn boxlite run -d");
    let run_stdout = String::from_utf8_lossy(&run_output.stdout).to_string();
    let run_stderr = String::from_utf8_lossy(&run_output.stderr).to_string();

    assert!(
        run_output.status.success(),
        "boxlite run -d redis:alpine exited non-zero — the EXPOSE \
         auto-remap path is broken (or never wired up).\n\
         exit code: {rc:?}\nstdout:\n{run_stdout}\nstderr:\n{run_stderr}",
        rc = run_output.status.code(),
    );

    let box_id = run_stdout.trim().to_string();
    assert!(
        !box_id.is_empty(),
        "boxlite run -d returned an empty box id (stderr:\n{run_stderr})"
    );
    eprintln!("box id: {box_id}");

    // Make sure the box gets torn down even if a later assertion panics.
    // The CLI handles SIGKILL of orphaned libkrun children cleanly.
    struct Cleanup<'a>(&'a common::TestContext, &'a str);
    impl Drop for Cleanup<'_> {
        fn drop(&mut self) {
            let _ = self.0.new_cmd().args(["rm", "--force", self.1]).output();
        }
    }
    let _cleanup = Cleanup(&ctx, &box_id);

    // `BoxState::port_mappings` is written at the Running transition
    // (see `box_impl.rs::run`), so by the time `run -d` returned,
    // `boxlite inspect` already has the resolved mapping.
    let inspect_output = ctx
        .new_cmd()
        .args(["inspect", &box_id])
        .output()
        .expect("spawn boxlite inspect");
    let inspect_stdout = String::from_utf8_lossy(&inspect_output.stdout).to_string();
    let inspect_stderr = String::from_utf8_lossy(&inspect_output.stderr).to_string();
    eprintln!("=== inspect stdout ===\n{inspect_stdout}\n=== end ===");

    assert!(
        inspect_output.status.success(),
        "boxlite inspect exited non-zero — stderr:\n{inspect_stderr}"
    );

    let parsed: Value =
        serde_json::from_str(inspect_stdout.trim()).expect("inspect output must be valid JSON");
    let arr = parsed
        .as_array()
        .expect("inspect output must be a JSON array");
    assert_eq!(arr.len(), 1, "single box → array of one");
    let ports = arr[0]
        .get("Ports")
        .and_then(|p| p.as_array())
        .unwrap_or_else(|| {
            panic!("inspect output must include a `Ports` array; got:\n{inspect_stdout}")
        });

    let entry = ports
        .iter()
        .find(|m| m.get("GuestPort").and_then(|v| v.as_u64()) == Some(EXPOSE_PORT as u64))
        .unwrap_or_else(|| {
            panic!(
                "inspect Ports has no entry for guest port {EXPOSE_PORT}; \
                 got: {ports:#?}"
            )
        });

    let host_port = entry
        .get("HostPort")
        .and_then(|v| v.as_u64())
        .unwrap_or_else(|| panic!("Ports entry missing HostPort: {entry:#?}"))
        as u16;
    let source = entry
        .get("Source")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    assert_ne!(
        host_port, EXPOSE_PORT,
        "EXPOSE {EXPOSE_PORT} must have been auto-remapped (we pre-bound \
         0.0.0.0:{EXPOSE_PORT} before starting the box). inspect entry: \
         {entry:#?}",
    );
    assert_eq!(
        source, "auto_remap",
        "EXPOSE entry for guest:{EXPOSE_PORT} must be Source=auto_remap \
         (got {source:?}); entry: {entry:#?}",
    );
    eprintln!("[ok] inspect → host:{host_port} → guest:{EXPOSE_PORT} (auto_remap)");

    drop(blocker);
}

/// Scope guard: an explicit `-p HOST:GUEST` mapping is the user's deliberate
/// choice and MUST NOT go through the EXPOSE auto-remap path — its host port
/// is preserved and `inspect` tags it `Source=user` (never `auto_expose` /
/// `auto_remap`).
///
/// Two-side regression contract:
///   - Correct: user mapping kept as-is → `Source=user`.
///   - Broken (user `-p` routed through `resolve_expose_host_port`): a free
///     host port resolves to `AutoExpose`, so `Source` flips to `auto_expose`
///     → the `Source=user` assertion fails.
///
/// Uses `alpine:latest` (no `EXPOSE`) so the only host-side mapping is the
/// explicit `-p`, with no auto-publish entries to disambiguate.
const USER_GUEST_PORT: u16 = 18080;

#[test]
fn user_published_port_keeps_user_source() {
    // Pick a host port that's currently free (bind ephemeral, record, release
    // so the box can take it). Small TOCTOU window; acceptable for a test.
    let host_port = {
        let l = TcpListener::bind(("0.0.0.0", 0)).expect("bind ephemeral to find a free host port");
        let p = l.local_addr().expect("local_addr").port();
        drop(l);
        p
    };

    let ctx = common::boxlite();

    let mapping = format!("{host_port}:{USER_GUEST_PORT}");
    let run_output = ctx
        .new_cmd()
        .timeout(Duration::from_secs(120))
        .args(["run", "-d", "-p", &mapping, "alpine:latest", "sleep", "300"])
        .output()
        .expect("spawn boxlite run -d");
    let run_stdout = String::from_utf8_lossy(&run_output.stdout).to_string();
    let run_stderr = String::from_utf8_lossy(&run_output.stderr).to_string();
    assert!(
        run_output.status.success(),
        "boxlite run -d -p {mapping} alpine failed:\nrc: {rc:?}\nstdout:\n{run_stdout}\nstderr:\n{run_stderr}",
        rc = run_output.status.code(),
    );

    let box_id = run_stdout.trim().to_string();
    assert!(
        !box_id.is_empty(),
        "boxlite run -d returned an empty box id (stderr:\n{run_stderr})"
    );

    struct Cleanup<'a>(&'a common::TestContext, &'a str);
    impl Drop for Cleanup<'_> {
        fn drop(&mut self) {
            let _ = self.0.new_cmd().args(["rm", "--force", self.1]).output();
        }
    }
    let _cleanup = Cleanup(&ctx, &box_id);

    let inspect_output = ctx
        .new_cmd()
        .args(["inspect", &box_id])
        .output()
        .expect("spawn boxlite inspect");
    let inspect_stdout = String::from_utf8_lossy(&inspect_output.stdout).to_string();
    assert!(
        inspect_output.status.success(),
        "boxlite inspect exited non-zero — stderr:\n{}",
        String::from_utf8_lossy(&inspect_output.stderr)
    );

    let parsed: Value =
        serde_json::from_str(inspect_stdout.trim()).expect("inspect output must be valid JSON");
    let arr = parsed
        .as_array()
        .expect("inspect output must be a JSON array");
    let ports = arr[0]
        .get("Ports")
        .and_then(|p| p.as_array())
        .unwrap_or_else(|| {
            panic!("inspect output must include a `Ports` array; got:\n{inspect_stdout}")
        });

    let entry = ports
        .iter()
        .find(|m| m.get("GuestPort").and_then(|v| v.as_u64()) == Some(USER_GUEST_PORT as u64))
        .unwrap_or_else(|| {
            panic!("inspect Ports has no entry for guest port {USER_GUEST_PORT}; got: {ports:#?}")
        });

    let got_host = entry
        .get("HostPort")
        .and_then(|v| v.as_u64())
        .unwrap_or_else(|| panic!("Ports entry missing HostPort: {entry:#?}")) as u16;
    let source = entry
        .get("Source")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    assert_eq!(
        got_host, host_port,
        "explicit -p host port must be preserved, not remapped; entry: {entry:#?}",
    );
    assert_eq!(
        source, "user",
        "explicit -p mapping must be Source=user, never run through the EXPOSE \
         auto-remap path (got {source:?}); entry: {entry:#?}",
    );
    eprintln!("[ok] user -p host:{host_port} → guest:{USER_GUEST_PORT} (source=user, not remapped)");
}

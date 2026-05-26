//! Regression: when a host port is already bound by another process,
//! `boxlite run -p HOST:GUEST` must fail-fast with a Rust-layer error
//! that names gvproxy as the failure source — not silently boot a box
//! with a dead netstack whose breakage only surfaces 20s+ later as a
//! guest "DNS lookup … i/o timeout".
//!
//! Pre-fix:
//! `virtualnetwork.New(tapConfig)` at
//! `src/deps/libgvproxy-sys/gvproxy-bridge/main.go:412-418` returned
//! the bind error to the surrounding goroutine, which logged it to
//! logrus and returned. `gvproxy_create` had already returned a valid
//! id by then, so the FFI caller never learned about the failure.
//!
//! Fix: surface the result via an `initErr` channel so `gvproxy_create`
//! returns -1 on bind failure and the Rust runtime maps that to
//! `Network("gvproxy_create failed")`.

mod common;

use std::net::TcpListener;
use std::process::Command;
use std::time::Instant;

#[test]
fn gvproxy_port_conflict_fails_fast_with_named_error() {
    // Plain TcpListener (no boxlite involvement) holds the host port
    // for the test's lifetime; OS picks a free ephemeral port so the
    // test is parallel-safe with everything.
    let holder = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    let host_port = holder.local_addr().unwrap().port();

    let ctx = common::boxlite();
    let started_at = Instant::now();

    // Bypass `assert_cmd`'s success-asserting wrappers — we expect
    // non-zero exit here and want the raw `Output`.
    let output = Command::new(env!("CARGO_BIN_EXE_boxlite"))
        .arg("--home")
        .arg(&ctx.home)
        .args(
            boxlite_test_utils::TEST_REGISTRIES
                .iter()
                .flat_map(|r| ["--registry", r]),
        )
        .args([
            "run",
            "--rm",
            "-p",
            &format!("{host_port}:80"),
            "alpine:latest",
            "true",
        ])
        .output()
        .expect("spawn boxlite");

    let elapsed = started_at.elapsed();
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);

    drop(holder); // release the held port after we've captured the box exit

    assert!(
        !output.status.success(),
        "boxlite must exit non-zero when -p host port is already bound\n\
         elapsed: {elapsed:?}\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stderr.contains("gvproxy_create failed"),
        "stderr must name gvproxy as the failure source; got:\n{stderr}"
    );
}

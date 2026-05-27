//! E2E test: `--kernel net` selects the net kernel blob which includes
//! netfilter/iptables modules. The lean kernel does not have them.
//!
//! This test requires the binary to be built with `--features kernel-net`
//! (or `kernel-lean,kernel-net` for dual mode). If the net blob is not
//! embedded, the test is skipped.

use assert_cmd::Command;
use boxlite_test_utils::home::PerTestBoxHome;
use std::time::Duration;

/// Returns (stdout, stderr). Skip detection (--kernel feature not built in)
/// surfaces in stderr because boxlite's failure path logs via tracing, not
/// stdout.
fn run_in_box(home: &PerTestBoxHome, kernel: Option<&str>, script: &str) -> (String, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_boxlite"));
    cmd.arg("--home")
        .arg(&home.path)
        .arg("--registry")
        .arg("docker.m.daocloud.io")
        .timeout(Duration::from_secs(120));

    let mut args = vec!["run", "--memory", "512"];
    if let Some(k) = kernel {
        args.push("--kernel");
        args.push(k);
    }
    args.extend(["alpine:latest", "sh", "-c", script]);
    cmd.args(&args);

    let output = cmd.output().expect("failed to execute boxlite");
    (
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

#[test]
fn kernel_net_has_iptables() {
    let home = PerTestBoxHome::new();

    let (stdout, stderr) = run_in_box(
        &home,
        Some("net"),
        "cat /proc/net/ip_tables_names 2>/dev/null && echo IPTABLES_OK || echo NO_IPTABLES",
    );

    // Binary built without `--features kernel-net` surfaces the dependency
    // requirement on stderr via tracing; skip rather than fail.
    if stderr.contains("--kernel net requires") || stdout.contains("--kernel net requires") {
        eprintln!("SKIP kernel_net_has_iptables: binary not built with kernel-net feature");
        return;
    }

    assert!(
        stdout.contains("IPTABLES_OK"),
        "net kernel must have iptables support; got stdout: {stdout}\nstderr: {stderr}"
    );
}

#[test]
fn kernel_lean_no_iptables() {
    let home = PerTestBoxHome::new();

    let (stdout, stderr) = run_in_box(
        &home,
        None,
        "cat /proc/net/ip_tables_names 2>/dev/null && echo IPTABLES_OK || echo NO_IPTABLES",
    );

    assert!(
        stdout.contains("NO_IPTABLES"),
        "lean kernel must NOT have iptables; got stdout: {stdout}\nstderr: {stderr}"
    );
}

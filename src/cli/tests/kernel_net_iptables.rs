//! E2E test: `--kernel net` selects the net kernel blob which includes
//! netfilter/iptables modules. The lean kernel does not have them.
//!
//! This test requires the binary to be built with `--features kernel-net`
//! (or `kernel-lean,kernel-net` for dual mode).

use assert_cmd::Command;
use boxlite_test_utils::home::PerTestBoxHome;
use std::time::Duration;

/// Returns (stdout, stderr).
fn run_in_box(home: &PerTestBoxHome, kernel: Option<&str>, script: &str) -> (String, String) {
    let image =
        std::env::var("BOXLITE_KERNEL_TEST_IMAGE").unwrap_or_else(|_| "alpine:latest".to_string());
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_boxlite"));
    cmd.arg("--home")
        .arg(&home.path)
        .arg("--registry")
        .arg("docker.m.daocloud.io")
        .timeout(Duration::from_secs(120));

    let mut args = vec!["run", "--memory", "512", "--entrypoint", "sh"];
    if let Some(k) = kernel {
        args.push("--kernel");
        args.push(k);
    }
    args.extend([image.as_str(), "-c", script]);
    cmd.args(&args);

    let output = cmd.output().expect("failed to execute boxlite");
    (
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

#[test]
#[cfg(feature = "kernel-net")]
fn kernel_net_has_iptables() {
    let home = PerTestBoxHome::new();

    let (stdout, stderr) = run_in_box(
        &home,
        Some("net"),
        "cat /proc/net/ip_tables_names 2>/dev/null && echo IPTABLES_OK || echo NO_IPTABLES",
    );

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

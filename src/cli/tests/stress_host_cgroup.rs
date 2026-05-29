//! Integration test: rootless host cgroup enforcement.
//!
//! On a rootless host the shim can't migrate itself across the root-owned
//! user.slice into a limited cgroup, so boxlite asks the systemd user manager
//! to adopt the running shim into a transient `boxlite-<id>.scope` carrying
//! `MemoryMax` (2×VM + 512 MiB headroom) and `TasksMax` (1024). This bounds the
//! host RAM / PID blast radius of a single box. Without that adoption the shim
//! stays in the unconstrained login `session-N.scope` (MemoryMax=infinity),
//! which is exactly what this test fails on.
//!
//! Skips when running as root (which uses the direct-cgroup path, not a scope)
//! or when there is no systemd user manager to query.

use assert_cmd::Command;
use boxlite_test_utils::home::PerTestBoxHome;
use std::path::Path;
use std::time::Duration;

fn boxlite(home: &Path, args: &[&str], timeout: Duration) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_boxlite"))
        .arg("--home")
        .arg(home)
        .args(args)
        .timeout(timeout)
        .output()
        .expect("spawn boxlite")
}

struct BoxCleanup {
    home: std::path::PathBuf,
    id: String,
}
impl Drop for BoxCleanup {
    fn drop(&mut self) {
        let _ = boxlite(&self.home, &["rm", "-f", &self.id], Duration::from_secs(30));
    }
}

/// `systemctl --user show <unit> -p <prop>` → the property's value, or None if
/// systemctl/the user manager isn't usable here.
fn systemctl_show(unit: &str, prop: &str) -> Option<String> {
    let out = std::process::Command::new("systemctl")
        .args(["--user", "show", unit, "-p", prop])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&out.stdout);
    line.trim()
        .strip_prefix(&format!("{prop}="))
        .map(|v| v.trim().to_string())
}

fn is_root() -> bool {
    std::process::Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "0")
        .unwrap_or(false)
}

#[test]
fn shim_is_scoped_with_host_memory_and_pids_limits() {
    if is_root() {
        eprintln!("SKIP: running as root uses the direct cgroup path, not a systemd scope");
        return;
    }
    // No usable user manager → nothing to assert against.
    if systemctl_show("init.scope", "MemoryMax").is_none() {
        eprintln!("SKIP: no systemd --user manager to query");
        return;
    }

    let home = PerTestBoxHome::new();
    let out = boxlite(
        home.path.as_path(),
        &[
            "--registry",
            "docker.m.daocloud.io",
            "run",
            "-d",
            "--memory",
            "128",
            "alpine:latest",
            "sleep",
            "600",
        ],
        Duration::from_secs(300),
    );
    assert!(
        out.status.success(),
        "box start failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let box_id = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let _cleanup = BoxCleanup {
        home: home.path.clone(),
        id: box_id.clone(),
    };

    let unit = format!("boxlite-{box_id}.scope");
    // 2×128 MiB VM + 512 MiB headroom — the documented host memory cap.
    let expected_mem = (128u64 * 2 * 1024 * 1024) + (512 * 1024 * 1024);

    let mem = systemctl_show(&unit, "MemoryMax").unwrap_or_default();
    let tasks = systemctl_show(&unit, "TasksMax").unwrap_or_default();

    assert_eq!(
        mem,
        expected_mem.to_string(),
        "shim must be scoped with MemoryMax={expected_mem}; got {mem:?} for {unit} \
         (an unscoped shim reports MemoryMax=infinity — limits not enforced)"
    );
    assert_eq!(
        tasks, "1024",
        "shim scope must cap TasksMax at 1024; got {tasks:?} for {unit}"
    );
}

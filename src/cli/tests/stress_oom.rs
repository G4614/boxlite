//! Integration stress test: container cgroup `memory.max` + `pids.max` keep a
//! hostile workload from panicking the guest VM kernel.
//!
//! One 128 MB box is driven through three escalating attack waves, each of
//! which tends to take down an unprotected tiny VM: a pids bomb (fork 1000 idle
//! children, which `pids.max` = 512 must cap before PID/kernel-memory
//! exhaustion), a memory bomb (one child mallocs+touches 512 MB — 4× the VM —
//! which `memory.max` must OOM-kill on its own), and a combined wave (200
//! children each touching 2 MB). After every wave the VM must still be
//! `Running` and accept a fresh exec, proving the cgroup OOM killer hit only
//! container processes — never the guest kernel or agent.
//!
//! Requires a VM-capable host with network to pull `alpine` and `apk add gcc`.

use assert_cmd::Command;
use boxlite_test_utils::home::PerTestBoxHome;
use std::path::Path;
use std::time::Duration;

/// `boxlite --home <home> <args...>` with a timeout.
fn boxlite(home: &Path, args: &[&str], timeout: Duration) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_boxlite"))
        .arg("--home")
        .arg(home)
        .args(args)
        .timeout(timeout)
        .output()
        .expect("spawn boxlite")
}

/// `boxlite exec <box> -- sh -c <script>`.
fn exec_sh(home: &Path, box_id: &str, script: &str, timeout: Duration) -> std::process::Output {
    boxlite(home, &["exec", box_id, "--", "sh", "-c", script], timeout)
}

/// The VM is healthy iff it still lists as `Running` and a fresh exec succeeds.
/// A guest-kernel panic or dead agent fails one or both.
fn assert_vm_survives(home: &Path, box_id: &str, ctx: &str) {
    let list = boxlite(home, &["list"], Duration::from_secs(15));
    let listing = String::from_utf8_lossy(&list.stdout);
    assert!(
        listing.contains("Running"),
        "VM must survive {ctx}, but it is no longer Running; `list` =\n{listing}"
    );
    let echo = boxlite(
        home,
        &["exec", box_id, "--", "echo", "alive"],
        Duration::from_secs(15),
    );
    assert!(
        echo.status.success(),
        "guest agent must accept exec after {ctx}; stderr = {}",
        String::from_utf8_lossy(&echo.stderr)
    );
}

/// flood <n> <mb>: fork n children; each (if mb>0) malloc+memset mb MiB to fault
/// it in as RSS, then sleep briefly and exit so waves don't accumulate. The
/// parent prints how many forks actually succeeded (fewer than n once pids.max
/// blocks them) and exits immediately.
const FLOOD_C: &str = r#"
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <stdio.h>
int main(int c, char **v) {
    int n = c > 1 ? atoi(v[1]) : 100;
    long mb = c > 2 ? atol(v[2]) : 2;
    int i, forked = 0;
    for (i = 0; i < n; i++) {
        pid_t p = fork();
        if (p == 0) {
            if (mb > 0) { char *m = malloc((size_t)mb << 20); if (m) memset(m, 66, (size_t)mb << 20); }
            sleep(20);
            _exit(0);
        }
        if (p > 0) forked++;
    }
    printf("forked %d/%d x %ldMB\n", forked, n, mb);
    fflush(stdout);
    return 0;
}
"#;

/// Run one attack wave, then assert the VM survived and reset the box for the
/// next wave by killing any lingering flood children.
fn run_wave(home: &Path, box_id: &str, n: u32, mb: u32, ctx: &str) {
    // The parent exits right after forking; give the cgroup a moment to OOM-kill
    // / block, then assert survival before cleaning up.
    let _ = exec_sh(
        home,
        box_id,
        &format!("/tmp/flood {n} {mb} || true"),
        Duration::from_secs(30),
    );
    std::thread::sleep(Duration::from_secs(8));
    assert_vm_survives(home, box_id, ctx);
    // Reap survivors so the next wave starts from a clean slate.
    let _ = exec_sh(
        home,
        box_id,
        "pkill -9 -x flood || true",
        Duration::from_secs(15),
    );
    std::thread::sleep(Duration::from_secs(2));
}

/// 128 MB box, gcc-compiled flood, driven through pids → memory → combined
/// attack waves. Without the cgroup limits a tiny VM panics under these; with
/// `memory.max` + `pids.max` it survives every wave.
#[test]
fn cgroup_limits_keep_vm_alive_under_pids_and_memory_bombs() {
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

    // Toolchain to build the flood in-box (a real source of memory pressure too).
    let install = exec_sh(
        home.path.as_path(),
        &box_id,
        "apk add -q --no-cache gcc musl-dev >/dev/null 2>&1 && echo ok",
        Duration::from_secs(180),
    );
    assert!(
        String::from_utf8_lossy(&install.stdout).contains("ok"),
        "gcc install failed: {}",
        String::from_utf8_lossy(&install.stderr)
    );

    let compile = format!(
        "cat > /tmp/flood.c << 'CEOF'\n{FLOOD_C}\nCEOF\ngcc -O0 -o /tmp/flood /tmp/flood.c && echo compiled"
    );
    let out = exec_sh(
        home.path.as_path(),
        &box_id,
        &compile,
        Duration::from_secs(90),
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("compiled"),
        "flood compile failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Wave 1 — pids bomb: 1000 idle forks vs pids.max = 512.
    run_wave(
        home.path.as_path(),
        &box_id,
        1000,
        0,
        "a 1000-fork pids bomb",
    );
    // Wave 2 — memory bomb: one process grabs 512 MB in a 128 MB VM.
    run_wave(
        home.path.as_path(),
        &box_id,
        1,
        512,
        "a single 512 MB allocation",
    );
    // Wave 3 — combined: 200 children each touch 2 MB.
    run_wave(
        home.path.as_path(),
        &box_id,
        200,
        2,
        "a 200×2 MB fork+alloc bomb",
    );
}

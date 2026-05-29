//! Integration tests for the container cgroup resource guard (`memory.max` +
//! `pids.max`) that keeps a hostile workload from taking down the guest VM.
//!
//! Two angles, both against a real 128 MB box:
//!   - enforcement — the limits are actually written to the container's own
//!     cgroup (`/boxlite/<id>/memory.max` bounded, `pids.max` = 512), so a
//!     regression that drops the resources or re-disables cgroups (leaving the
//!     container in the root) is caught directly.
//!   - survival — the VM stays `Running` and keeps serving exec through three
//!     escalating attack waves (a 1000-fork pids bomb, a single 512 MB malloc
//!     in a 128 MB VM, and a 200×2 MB fork+alloc bomb).
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

/// Force-removes a box on drop so the `PerTestBoxHome` live-shim guard can't
/// fire and mask the real failure. Declare it *after* the home so it drops
/// first (reverse declaration order).
struct BoxCleanup {
    home: std::path::PathBuf,
    id: String,
}
impl Drop for BoxCleanup {
    fn drop(&mut self) {
        let _ = boxlite(&self.home, &["rm", "-f", &self.id], Duration::from_secs(30));
    }
}

/// Start a detached 128 MB alpine box running `sleep 600`; returns its id.
fn start_128mb_box(home: &Path) -> String {
    let out = boxlite(
        home,
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
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Directly verifies the limits are applied to the container's own cgroup: a
/// `/boxlite/<id>` path with `memory.max` bounded below the VM and `pids.max`
/// = 512. Guards both the explicit cgroups_path and that the resources are
/// written — dropping either (or re-disabling cgroups, which puts the
/// container back in the root) fails here.
#[test]
fn cgroup_limits_are_enforced_on_the_container() {
    let home = PerTestBoxHome::new();
    let box_id = start_128mb_box(home.path.as_path());
    let _cleanup = BoxCleanup {
        home: home.path.clone(),
        id: box_id.clone(),
    };

    // Read the container's own cgroup path and its controller files.
    let out = exec_sh(
        home.path.as_path(),
        &box_id,
        "cg=$(sed 's/^0:://' /proc/self/cgroup); \
         printf 'CG=%s\\nMEM=%s\\nPIDS=%s\\n' \
           \"$cg\" \
           \"$(cat /sys/fs/cgroup$cg/memory.max 2>&1)\" \
           \"$(cat /sys/fs/cgroup$cg/pids.max 2>&1)\"",
        Duration::from_secs(30),
    );
    let report = String::from_utf8_lossy(&out.stdout);
    let field = |key: &str| -> String {
        report
            .lines()
            .find_map(|l| l.strip_prefix(key))
            .unwrap_or("")
            .trim()
            .to_string()
    };

    let cg = field("CG=");
    let mem = field("MEM=");
    let pids = field("PIDS=");

    assert!(
        cg.starts_with("/boxlite/"),
        "container must run in its own /boxlite/<id> cgroup, not the root; got {cg:?}\n{report}"
    );
    let mem_max: u64 = mem.parse().unwrap_or_else(|_| {
        panic!("memory.max must be a concrete byte cap, not {mem:?} (cgroup not applied)\n{report}")
    });
    assert!(
        mem_max > 0 && mem_max < 128 * 1024 * 1024,
        "memory.max ({mem_max}) must be a positive cap below the 128 MB VM\n{report}"
    );
    assert_eq!(
        pids, "512",
        "pids.max must be the CONTAINER_PIDS_MAX ceiling\n{report}"
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

/// Run one attack wave, then assert the VM survived and reset the box for the
/// next wave by killing any lingering flood children. Returns the flood's
/// stdout (`forked <succeeded>/<requested> x <mb>MB`) so a caller can assert on
/// how many forks the cgroup actually let through.
fn run_wave(home: &Path, box_id: &str, n: u32, mb: u32, ctx: &str) -> String {
    // The parent exits right after forking; give the cgroup a moment to OOM-kill
    // / block, then assert survival before cleaning up.
    let out = exec_sh(
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
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Parse the `forked <succeeded>/<requested>` count the flood prints.
fn forked_count(wave_stdout: &str) -> u32 {
    wave_stdout
        .lines()
        .find_map(|l| l.strip_prefix("forked "))
        .and_then(|rest| rest.split('/').next())
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or_else(|| panic!("flood did not report a fork count:\n{wave_stdout}"))
}

/// 128 MB box, gcc-compiled flood, driven through pids → memory → combined
/// attack waves. The container's `memory.max` + `pids.max` keep the OOM kills
/// scoped to container processes so the guest kernel and agent stay up.
#[test]
fn cgroup_limits_keep_vm_alive_under_pids_and_memory_bombs() {
    let home = PerTestBoxHome::new();
    let box_id = start_128mb_box(home.path.as_path());
    let _cleanup = BoxCleanup {
        home: home.path.clone(),
        id: box_id.clone(),
    };

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
    let wave1 = run_wave(
        home.path.as_path(),
        &box_id,
        1000,
        0,
        "a 1000-fork pids bomb",
    );
    // Survival alone can't tell an enforced cap from a kernel that happened to
    // cope: assert pids.max actually blocked the bomb. The container can hold
    // at most 512 tasks (CONTAINER_PIDS_MAX) including the ones already running,
    // so far fewer than the requested 1000 forks can succeed. 700 leaves slack
    // above the 512 ceiling while still proving hundreds of forks were refused.
    let forked = forked_count(&wave1);
    assert!(
        (1..700).contains(&forked),
        "pids.max must cap the fork bomb well below the requested 1000 \
         (ceiling 512 + already-running tasks); got forked={forked}\n{wave1}"
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

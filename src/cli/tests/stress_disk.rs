//! Integration test: a box's writable rootfs is a bounded, isolated disk.
//!
//! A box must not see or be able to exhaust the host filesystem, and filling
//! its own rootfs to ENOSPC must not take down the guest VM. boxlite sizes the
//! per-box ext4/qcow2 overlay from the image (a few hundred MB for alpine), far
//! below the host disk — so a runaway writer inside a box is capped at its own
//! image size, and the host blast radius is bounded by that, not the host's
//! free space.
//!
//! Requires a VM-capable host with network to pull `alpine`.

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

fn exec_sh(home: &Path, box_id: &str, script: &str, timeout: Duration) -> std::process::Output {
    boxlite(home, &["exec", box_id, "--", "sh", "-c", script], timeout)
}

/// Force-removes the box on drop so the `PerTestBoxHome` live-shim guard can't
/// fire and mask the real failure (declared after `home`, so it drops first).
struct BoxCleanup {
    home: std::path::PathBuf,
    id: String,
}
impl Drop for BoxCleanup {
    fn drop(&mut self) {
        let _ = boxlite(&self.home, &["rm", "-f", &self.id], Duration::from_secs(30));
    }
}

/// Start a detached 256 MB alpine box running `sleep 600`; returns its id.
fn start_box(home: &Path) -> String {
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

/// The box is healthy iff a fresh exec still succeeds.
fn assert_alive(home: &Path, box_id: &str, ctx: &str) {
    let echo = boxlite(
        home,
        &["exec", box_id, "--", "echo", "alive"],
        Duration::from_secs(15),
    );
    assert!(
        echo.status.success(),
        "VM must stay alive {ctx}; stderr = {}",
        String::from_utf8_lossy(&echo.stderr)
    );
}

/// A box's rootfs is its own small disk (not the host's), and filling it to
/// ENOSPC leaves the VM alive and serving.
#[test]
fn box_rootfs_is_bounded_isolated_and_survives_fill() {
    let home = PerTestBoxHome::new();
    let out = boxlite(
        home.path.as_path(),
        &[
            "--registry",
            "docker.m.daocloud.io",
            "run",
            "-d",
            "--memory",
            "256",
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

    // Isolation + bound: the box's `/` is its own ext4 (1K-blocks total), sized
    // from the image — far below the host disk. A box that saw the host fs would
    // report tens of millions of 1K-blocks (e.g. a 124 GiB host ≈ 130M blocks).
    let df = exec_sh(
        home.path.as_path(),
        &box_id,
        "df -P / | awk 'NR==2{print $2}'", // 1K-blocks total of the rootfs
        Duration::from_secs(20),
    );
    let total_kb: u64 = String::from_utf8_lossy(&df.stdout)
        .trim()
        .parse()
        .unwrap_or_else(|_| {
            panic!(
                "could not read box rootfs size: {:?}",
                String::from_utf8_lossy(&df.stdout)
            )
        });
    assert!(
        (1..4 * 1024 * 1024).contains(&total_kb),
        "box rootfs must be its own bounded disk (a few hundred MB), not the host fs; \
         got {total_kb} 1K-blocks (~{} MiB)",
        total_kb / 1024
    );

    // Fill the rootfs: the write must hit ENOSPC, not hang or wander onto the
    // host disk. `dd` reports "No space left on device" on the bounded ext4.
    let fill = exec_sh(
        home.path.as_path(),
        &box_id,
        "dd if=/dev/zero of=/fill bs=1M 2>&1; true",
        Duration::from_secs(120),
    );
    let fill_out = String::from_utf8_lossy(&fill.stdout) + String::from_utf8_lossy(&fill.stderr);
    assert!(
        fill_out.contains("No space left"),
        "filling the rootfs must hit ENOSPC (bounded disk), got:\n{fill_out}"
    );

    // The VM survives a full rootfs: still Running and accepting exec.
    let list = boxlite(home.path.as_path(), &["list"], Duration::from_secs(15));
    assert!(
        String::from_utf8_lossy(&list.stdout).contains("Running"),
        "VM must stay Running after its rootfs fills; `list` =\n{}",
        String::from_utf8_lossy(&list.stdout)
    );
    let echo = boxlite(
        home.path.as_path(),
        &["exec", &box_id, "--", "echo", "alive"],
        Duration::from_secs(15),
    );
    assert!(
        echo.status.success(),
        "guest agent must accept exec after the rootfs fills; stderr = {}",
        String::from_utf8_lossy(&echo.stderr)
    );
}

/// Two boxes own independent rootfs disks: filling one to ENOSPC must not
/// shrink the other or stop it serving. "self-bounded" (above) plus this
/// "isolated from peers" check is what makes a box's disk a real per-box
/// resource boundary, not a shared pool.
#[test]
fn two_boxes_rootfs_disks_are_isolated() {
    let home = PerTestBoxHome::new();
    let victim = start_box(home.path.as_path());
    let _victim_cleanup = BoxCleanup {
        home: home.path.clone(),
        id: victim.clone(),
    };
    let bystander = start_box(home.path.as_path());
    let _bystander_cleanup = BoxCleanup {
        home: home.path.clone(),
        id: bystander.clone(),
    };

    // Record the bystander's free space before the victim runs amok.
    let avail = |box_id: &str| -> u64 {
        let out = exec_sh(
            home.path.as_path(),
            box_id,
            "df -P / | awk 'NR==2{print $4}'", // 1K-blocks available
            Duration::from_secs(20),
        );
        String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .unwrap_or_else(|_| {
                panic!(
                    "could not read free space for {box_id}: {:?}",
                    String::from_utf8_lossy(&out.stdout)
                )
            })
    };
    let bystander_free_before = avail(&bystander);

    // The victim fills its own rootfs to ENOSPC.
    let fill = exec_sh(
        home.path.as_path(),
        &victim,
        "dd if=/dev/zero of=/fill bs=1M 2>&1; true",
        Duration::from_secs(120),
    );
    let fill_out = String::from_utf8_lossy(&fill.stdout) + String::from_utf8_lossy(&fill.stderr);
    assert!(
        fill_out.contains("No space left"),
        "victim rootfs must fill to ENOSPC, got:\n{fill_out}"
    );

    // The bystander is untouched: a separate disk keeps essentially all its free
    // space (allow a small slack for its own logging) and still accepts writes.
    let bystander_free_after = avail(&bystander);
    assert!(
        bystander_free_after + 4096 >= bystander_free_before,
        "bystander free space must not shrink when a peer box fills its disk: \
         {bystander_free_before} KB before vs {bystander_free_after} KB after"
    );
    let write = exec_sh(
        home.path.as_path(),
        &bystander,
        "echo isolated > /probe && cat /probe",
        Duration::from_secs(20),
    );
    assert!(
        String::from_utf8_lossy(&write.stdout).contains("isolated"),
        "bystander box must still accept writes after a peer filled its disk; stderr = {}",
        String::from_utf8_lossy(&write.stderr)
    );

    // Both VMs survive.
    assert_alive(home.path.as_path(), &victim, "after filling its own rootfs");
    assert_alive(
        home.path.as_path(),
        &bystander,
        "while a peer box filled its disk",
    );
}

/// Concurrent writers in the *same* box: when several `dd` processes race to
/// fill the rootfs, every one of them must see a clean `No space left on
/// device` (no hangs, no silent partial success, no corruption that the VM
/// can't recover from), and the VM must stay alive. Guards against a bug
/// where one writer hitting ENOSPC could deadlock or wedge the rootfs for the
/// others — e.g. a stuck journal commit, an EXT4 lock pile-up, or a guest
/// agent that dies on the I/O storm.
#[test]
fn concurrent_writers_all_hit_enospc_and_vm_survives() {
    let home = PerTestBoxHome::new();
    let box_id = start_box(home.path.as_path());
    let _cleanup = BoxCleanup {
        home: home.path.clone(),
        id: box_id.clone(),
    };

    // Spawn N writers in parallel, each filling its own file. The shell waits
    // for all of them, then dumps every writer's stderr — so the aggregated
    // stdout carries exactly one ENOSPC message per writer if the rootfs
    // serialized them cleanly. Capture per-writer rc too, so a writer that
    // exited silently (not ENOSPC) shows up loudly.
    const N: u32 = 6;
    let script = format!(
        "for i in $(seq 1 {N}); do \
           ( dd if=/dev/zero of=/fill$i bs=1M 2>/tmp/dd-$i.err; \
             echo \"RC=$? FILE=/fill$i\" >>/tmp/dd-$i.err ) & \
         done; wait; \
         for i in $(seq 1 {N}); do \
           echo \"--- writer $i ---\"; cat /tmp/dd-$i.err; \
         done"
    );
    let out = exec_sh(
        home.path.as_path(),
        &box_id,
        &script,
        Duration::from_secs(180),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);

    // Every writer must hit ENOSPC. `dd` prints exactly one "No space left on
    // device" per writer when the rootfs is full; ≥ N occurrences proves every
    // one was rejected by the bounded ext4 rather than hanging or silently
    // succeeding.
    let enospc = stdout.matches("No space left").count();
    assert!(
        enospc >= N as usize,
        "every one of the {N} concurrent writers must hit ENOSPC; saw {enospc} \
         ENOSPC messages\n{stdout}"
    );

    // Every writer must have exited non-zero (dd returns 1 on write error).
    // RC=0 would mean a writer somehow succeeded with the disk full.
    let rc_zero = stdout.matches("RC=0 ").count();
    assert_eq!(
        rc_zero, 0,
        "no concurrent writer may exit RC=0 while the rootfs is full;\n{stdout}"
    );

    // The VM and the guest agent survive the concurrent I/O storm + ENOSPC.
    assert_alive(
        home.path.as_path(),
        &box_id,
        "after a concurrent fill of the rootfs",
    );
}

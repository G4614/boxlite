//! Integration test: `-v <vol>:size=N` caps the volume at N bytes.
//!
//! Architecture (end-to-end):
//!   boxlite host: `-v /data:size=64M` → resolve_user_volumes materialises
//!     `<box_home>/volumes/uservol0.img` (sparse + mkfs.ext4 sized to 64 MiB).
//!   libkrun: image attached as another `/dev/vdN`.
//!   guest agent: BlockDeviceMount picks it up + mounts at `/data`.
//!   box: `/data` is a 64-MiB ext4. `dd` past the cap → ENOSPC at the
//!     volume's own kernel boundary; rootfs and host fs untouched.

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

#[test]
fn sized_volume_caps_writes_and_rm_cleans_up_image() {
    let home = PerTestBoxHome::new();

    // 64 MiB volume: well above MIN_SIZED_VOLUME_BYTES (16) but small enough
    // to fill in seconds. Anonymous volume (no host path) so boxlite manages
    // the backing image entirely.
    let out = boxlite(
        home.path.as_path(),
        &[
            "--registry",
            "docker.m.daocloud.io",
            "run",
            "-d",
            "--memory",
            "256",
            "-v",
            "/data:size=64M",
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

    // Inside the box: confirm /data is its own bounded ext4, then fill it.
    let probe = boxlite(
        home.path.as_path(),
        &[
            "exec",
            &box_id,
            "--",
            "sh",
            "-c",
            "df -P /data | awk 'NR==2{print \"SIZE_KB=\" $2}'; \
             dd if=/dev/zero of=/data/fill bs=1M 2>&1; true",
        ],
        Duration::from_secs(60),
    );
    let combined = String::from_utf8_lossy(&probe.stdout).to_string()
        + &String::from_utf8_lossy(&probe.stderr);

    // 1. Volume size: ext4 overhead on a 64 MiB image (journal + reserved
    //    blocks) lands the usable size around 50-64 MiB. NOT the host's
    //    tens-of-millions of 1K-blocks — that'd be the host fs leaking
    //    through, which the virtio-blk path forbids by construction.
    let size_kb: u64 = combined
        .lines()
        .find_map(|l| l.strip_prefix("SIZE_KB="))
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("no SIZE_KB line in output:\n{combined}"));
    assert!(
        (40 * 1024..=70 * 1024).contains(&size_kb),
        "volume size must be ≈ 64 MiB (after ext4 overhead); got {size_kb} KB \
         (~{} MiB)\n{combined}",
        size_kb / 1024
    );

    // 2. Fill must have hit ENOSPC at the volume's own ext4 boundary.
    assert!(
        combined.contains("No space left"),
        "fill must hit ENOSPC at the volume cap, not propagate past:\n{combined}"
    );

    // 3. Box survives — the fill stayed inside its own block device, agent
    //    still serving exec.
    let echo = boxlite(
        home.path.as_path(),
        &["exec", &box_id, "--", "echo", "alive"],
        Duration::from_secs(15),
    );
    assert!(
        echo.status.success(),
        "box must survive a sized-volume fill (it's an isolated block device); \
         stderr = {}",
        String::from_utf8_lossy(&echo.stderr)
    );

    // 4. Image file is at the conventional location AND rm cleans it up.
    let img = home
        .path
        .join("boxes")
        .join(&box_id)
        .join("volumes")
        .join("uservol0.img");
    assert!(
        img.exists(),
        "sized-volume image must live at {} while the box runs",
        img.display()
    );
    let rm = boxlite(
        home.path.as_path(),
        &["rm", "-f", &box_id],
        Duration::from_secs(60),
    );
    assert!(
        rm.status.success(),
        "rm failed: {}",
        String::from_utf8_lossy(&rm.stderr)
    );
    assert!(
        !img.exists(),
        "rm must delete the sized-volume image at {}",
        img.display()
    );
}

/// Two sized volumes on one box are independent — filling one to ENOSPC must
/// not shrink, corrupt, or unmount the other. Catches regressions in the
/// `uservol{i}` naming, the `/dev/vdN` index handoff, or any state the
/// volume-mgr loop shares incorrectly between entries.
#[test]
fn two_sized_volumes_on_one_box_are_independent() {
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
            "-v",
            "/a:size=32M",
            "-v",
            "/b:size=64M",
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

    // Both volumes mount at distinct caps. df reports 1K-blocks; after ext4
    // overhead /a (32 MiB) lands around 20-32 MiB, /b (64 MiB) around 40-64.
    // The key invariant is `/b` is roughly DOUBLE `/a` — a wiring mistake
    // that crossed devices would either fail to mount or show the same size.
    let sizes = boxlite(
        home.path.as_path(),
        &[
            "exec",
            &box_id,
            "--",
            "sh",
            "-c",
            "df -P /a | awk 'NR==2{print \"A_KB=\" $2}'; \
             df -P /b | awk 'NR==2{print \"B_KB=\" $2}'",
        ],
        Duration::from_secs(20),
    );
    let stdout = String::from_utf8_lossy(&sizes.stdout);
    let parse = |key: &str| -> u64 {
        stdout
            .lines()
            .find_map(|l| l.strip_prefix(key))
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| panic!("missing {key} in:\n{stdout}"))
    };
    let a_kb = parse("A_KB=");
    let b_kb = parse("B_KB=");
    assert!(
        (15 * 1024..=35 * 1024).contains(&a_kb),
        "/a (size=32M) must be ≈ 32 MiB; got {a_kb} KB\n{stdout}"
    );
    assert!(
        (40 * 1024..=70 * 1024).contains(&b_kb),
        "/b (size=64M) must be ≈ 64 MiB; got {b_kb} KB\n{stdout}"
    );
    assert!(
        b_kb > a_kb + 10 * 1024,
        "/b must be visibly larger than /a (independent devices, not crossed); \
         got A={a_kb} B={b_kb}"
    );

    // Fill /a to ENOSPC. /b must be completely unaffected — read original
    // available bytes, fill /a, re-read /b, expect ~no change.
    let b_avail_before = {
        let o = boxlite(
            home.path.as_path(),
            &[
                "exec",
                &box_id,
                "--",
                "sh",
                "-c",
                "df -P /b | awk 'NR==2{print $4}'",
            ],
            Duration::from_secs(20),
        );
        String::from_utf8_lossy(&o.stdout)
            .trim()
            .parse::<u64>()
            .unwrap_or(0)
    };

    let fill = boxlite(
        home.path.as_path(),
        &[
            "exec",
            &box_id,
            "--",
            "sh",
            "-c",
            "dd if=/dev/zero of=/a/fill bs=1M 2>&1; true",
        ],
        Duration::from_secs(60),
    );
    let fill_out =
        String::from_utf8_lossy(&fill.stdout).to_string() + &String::from_utf8_lossy(&fill.stderr);
    assert!(
        fill_out.contains("No space left"),
        "/a fill must hit ENOSPC at its own cap; got:\n{fill_out}"
    );

    // /b: still mounted, still has roughly the same free space.
    let after = boxlite(
        home.path.as_path(),
        &[
            "exec",
            &box_id,
            "--",
            "sh",
            "-c",
            "df -P /b | awk 'NR==2{print $4}' && echo bystander > /b/probe && cat /b/probe",
        ],
        Duration::from_secs(20),
    );
    let out = String::from_utf8_lossy(&after.stdout);
    let b_avail_after: u64 = out
        .lines()
        .next()
        .and_then(|l| l.trim().parse().ok())
        .unwrap_or_else(|| panic!("/b df failed after /a fill:\n{out}"));
    assert!(
        b_avail_after + 1024 >= b_avail_before,
        "/b must not shrink when /a fills (separate devices): \
         before={b_avail_before} after={b_avail_after}"
    );
    assert!(
        out.contains("bystander"),
        "/b must still accept writes when /a is full; got:\n{out}"
    );
}

/// Data written into a sized volume survives a `stop`/`start` cycle — the
/// image is persistent on the host across box lifecycle transitions, and the
/// guest re-mounts it on next start. The user-most-likely-to-want behaviour.
#[test]
fn sized_volume_data_persists_across_stop_start() {
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
            "-v",
            "/data:size=32M",
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

    // Write a marker the test will look for after restart.
    let marker = "persisted-across-stop-start-cycle-7c9";
    let write = boxlite(
        home.path.as_path(),
        &[
            "exec",
            &box_id,
            "--",
            "sh",
            "-c",
            &format!("echo {marker} > /data/marker.txt && cat /data/marker.txt"),
        ],
        Duration::from_secs(30),
    );
    assert!(
        String::from_utf8_lossy(&write.stdout).contains(marker),
        "writing the marker must succeed in the fresh box; got:\n{}",
        String::from_utf8_lossy(&write.stdout)
    );

    // Stop the box, then start it back up.
    let stop = boxlite(
        home.path.as_path(),
        &["stop", &box_id],
        Duration::from_secs(60),
    );
    assert!(
        stop.status.success(),
        "stop failed: {}",
        String::from_utf8_lossy(&stop.stderr)
    );
    let start = boxlite(
        home.path.as_path(),
        &["start", &box_id],
        Duration::from_secs(180),
    );
    assert!(
        start.status.success(),
        "start failed after stop: {}",
        String::from_utf8_lossy(&start.stderr)
    );

    // The marker must still be there — sized volume is persistent storage,
    // not tmpfs.
    let read = boxlite(
        home.path.as_path(),
        &["exec", &box_id, "--", "cat", "/data/marker.txt"],
        Duration::from_secs(30),
    );
    let stdout = String::from_utf8_lossy(&read.stdout);
    assert!(
        stdout.contains(marker),
        "marker must persist across stop/start; got:\n{stdout}\nstderr={}",
        String::from_utf8_lossy(&read.stderr)
    );

    // The volume's cap must also persist — df still reports ~32 MiB.
    let size = boxlite(
        home.path.as_path(),
        &[
            "exec",
            &box_id,
            "--",
            "sh",
            "-c",
            "df -P /data | awk 'NR==2{print $2}'",
        ],
        Duration::from_secs(20),
    );
    let size_kb: u64 = String::from_utf8_lossy(&size.stdout)
        .trim()
        .parse()
        .unwrap_or_else(|_| panic!("could not read /data size after restart"));
    assert!(
        (15 * 1024..=35 * 1024).contains(&size_kb),
        "sized cap must persist; got {size_kb} KB after restart"
    );
}

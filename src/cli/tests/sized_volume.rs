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

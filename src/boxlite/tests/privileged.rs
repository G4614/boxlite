//! Integration tests for privileged mode: real capability and mount escalation.
//!
//! Verifies against an actual booted VM (not just the host-side resolution
//! logic already covered by `advanced_options.rs`'s unit tests):
//! 1. A privileged box's guest process actually gets a strictly larger
//!    effective capability set than a plain box's.
//! 2. A privileged box's `/sys` mount is actually writable; a plain box's
//!    stays read-only.

use boxlite::runtime::advanced_options::AdvancedBoxOptions;
use boxlite::runtime::options::{BoxOptions, RootfsSpec};
use boxlite_test_utils::box_test::BoxTestBase;

fn privileged_options() -> BoxOptions {
    let mut advanced = AdvancedBoxOptions::default();
    advanced.set_privileged(true);
    BoxOptions {
        rootfs: RootfsSpec::Image("alpine:latest".into()),
        auto_delete: Some(0),
        advanced,
        ..Default::default()
    }
}

fn plain_options() -> BoxOptions {
    BoxOptions {
        rootfs: RootfsSpec::Image("alpine:latest".into()),
        auto_delete: Some(0),
        ..Default::default()
    }
}

/// Parse the hex mask off a `CapEff:\t<hex>` line from `/proc/self/status`.
fn parse_cap_eff(line: &str) -> u64 {
    let hex = line
        .split_whitespace()
        .nth(1)
        .unwrap_or_else(|| panic!("no CapEff value in: {line}"));
    u64::from_str_radix(hex, 16).unwrap_or_else(|e| panic!("bad CapEff hex {hex:?}: {e}"))
}

/// Read the mount options field (4th column) for a mountpoint from `/proc/mounts`.
fn mount_options_for(proc_mounts: &str, mountpoint: &str) -> String {
    proc_mounts
        .lines()
        .find(|line| line.split_whitespace().nth(1) == Some(mountpoint))
        .unwrap_or_else(|| panic!("no {mountpoint} entry in /proc/mounts:\n{proc_mounts}"))
        .split_whitespace()
        .nth(3)
        .unwrap_or_else(|| panic!("malformed /proc/mounts line for {mountpoint}"))
        .to_string()
}

fn is_read_only(mount_options: &str) -> bool {
    mount_options.split(',').next() == Some("ro")
}

// Blocked on https://github.com/boxlite-ai/boxlite/issues/1349: the guest
// reports its version from the workspace Cargo.toml (currently 0.9.7), but
// guest_init.rs requires >= 0.9.8 for a privileged box (readonly_paths is
// empty exactly when privileged), so this can't pass in any environment
// until that version floor is resolved. Confirmed with a real VM boot, not
// simulated — the guest handshake fails with "guest 0.9.7 is older than the
// required 0.9.8". Un-ignore once #1349 lands.
#[tokio::test]
#[ignore = "blocked on #1349 - guest version floor (0.9.8) unsatisfiable while Cargo.toml is still 0.9.7"]
async fn privileged_box_gets_more_capabilities_and_writable_sys() {
    let privileged = BoxTestBase::with_options(privileged_options()).await;
    privileged.bx.start().await.unwrap();
    let plain = BoxTestBase::with_options(plain_options()).await;
    plain.bx.start().await.unwrap();

    let privileged_cap_eff = parse_cap_eff(
        &privileged
            .exec_stdout("sh", &["-c", "grep CapEff /proc/self/status"])
            .await,
    );
    let plain_cap_eff = parse_cap_eff(
        &plain
            .exec_stdout("sh", &["-c", "grep CapEff /proc/self/status"])
            .await,
    );
    assert!(
        privileged_cap_eff > plain_cap_eff,
        "privileged box's effective capabilities (0x{privileged_cap_eff:x}) should be a strict \
         superset of a plain box's (0x{plain_cap_eff:x})"
    );

    let privileged_sys_opts = mount_options_for(
        &privileged.exec_stdout("cat", &["/proc/mounts"]).await,
        "/sys",
    );
    assert!(
        !is_read_only(&privileged_sys_opts),
        "privileged box's /sys should be writable, got mount options: {privileged_sys_opts}"
    );

    let plain_sys_opts =
        mount_options_for(&plain.exec_stdout("cat", &["/proc/mounts"]).await, "/sys");
    assert!(
        is_read_only(&plain_sys_opts),
        "plain box's /sys should stay read-only, got mount options: {plain_sys_opts}"
    );
}

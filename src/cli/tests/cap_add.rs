//! Integration test: `--cap-add` end-to-end.
//!
//! Each test launches an alpine box, reads `CapEff:` from
//! `/proc/self/status` inside the container, and asserts on the exact
//! bitmask. CapEff is the kernel-visible effective capability set — the
//! final receiver of the chain that #597 plumbs:
//!
//!   CLI `--cap-add` → BoxOptions.added_caps → proto added_caps
//!     → guest build_capabilities → OCI Spec process.capabilities
//!     → libcontainer → /proc/self/status::CapEff
//!
//! Asserting at the kernel end catches a regression anywhere in that
//! chain, including ones that pass the parse-layer unit tests in
//! `cli.rs::cap_add_propagates_to_options`.

use assert_cmd::Command;
use std::time::Duration;

mod common;

/// Docker's well-known 14-cap default mask. Derived from the union of
/// CAP_CHOWN, DAC_OVERRIDE, FOWNER, FSETID, KILL, SETGID, SETUID, SETPCAP,
/// NET_BIND_SERVICE, NET_RAW, SYS_CHROOT, MKNOD, AUDIT_WRITE, SETFCAP
/// (bits 0,1,3,4,5,6,7,8,10,13,18,27,29,31). This is the same set boxlite's
/// `default_capabilities()` enumerates.
const DOCKER_DEFAULT_CAP_EFF: u64 = 0xa80425fb;

/// CAP_SYS_ADMIN bit position. Stable across kernel versions.
const CAP_SYS_ADMIN_BIT: u32 = 21;

struct BoxCleanup<'a> {
    ctx: &'a common::TestContext,
    id: String,
}
impl Drop for BoxCleanup<'_> {
    fn drop(&mut self) {
        self.ctx.cleanup_box(&self.id);
    }
}

/// Launch an alpine box (`sleep 600`) with the given extra args; return
/// the new TestContext + box_id. Caller wraps the id in BoxCleanup so
/// the box is rm-forced even on panic.
fn run_alpine(extra_args: &[&str]) -> (common::TestContext, String) {
    let ctx = common::boxlite();
    let mut run_cmd = ctx.new_cmd();
    run_cmd
        .args(["run", "-d", "--memory", "256"])
        .args(extra_args)
        .args(["alpine:latest", "sleep", "600"])
        .timeout(Duration::from_secs(300));
    let out = run_cmd.output().expect("spawn boxlite run");
    assert!(
        out.status.success(),
        "boxlite run {extra_args:?} failed: stderr = {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let box_id = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (ctx, box_id)
}

/// Exec `awk` inside the box, parse the returned hex into a u64.
/// Panics with the raw stdout/stderr on parse failure — the diagnostic
/// is what makes test failures here actionable.
fn read_cap_eff(ctx: &common::TestContext, box_id: &str) -> u64 {
    let mut exec_cmd: Command = ctx.new_cmd();
    exec_cmd
        .args([
            "exec",
            box_id,
            "--",
            "sh",
            "-c",
            "awk '/^CapEff:/ {print $2}' /proc/self/status",
        ])
        .timeout(Duration::from_secs(30));
    let out = exec_cmd.output().expect("spawn boxlite exec");
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    u64::from_str_radix(&stdout, 16).unwrap_or_else(|e| {
        panic!(
            "could not parse CapEff hex from /proc/self/status; \
             stdout={stdout:?} stderr={stderr:?}: {e}"
        )
    })
}

/// Baseline: an unmodified box's CapEff exactly matches Docker's default
/// 14-capability set.
///
/// Two failure modes this catches that the other tests can't:
///   - silent widening of the default set (e.g. an accidental SYS_ADMIN
///     in `default_capabilities()`),
///   - silent narrowing (e.g. a refactor dropping NET_RAW).
///
/// Without this baseline pinned, the "delta from default" reasoning in
/// the `--cap-add SYS_ADMIN` test below has no anchor — a regression that
/// silently shifted the default mask could leave both tests green.
#[test]
fn default_box_cap_eff_matches_docker_baseline() {
    let (ctx, box_id) = run_alpine(&[]);
    let _cleanup = BoxCleanup {
        ctx: &ctx,
        id: box_id.clone(),
    };
    let cap_eff = read_cap_eff(&ctx, &box_id);
    assert_eq!(
        cap_eff, DOCKER_DEFAULT_CAP_EFF,
        "default CapEff must match the docker 14-cap baseline; \
         expected 0x{DOCKER_DEFAULT_CAP_EFF:016x}, got 0x{cap_eff:016x}"
    );
}

/// `--cap-add SYS_ADMIN` must flip bit 21 on top of the default mask,
/// leaving every other bit unchanged.
///
/// Asserting on the exact resulting mask (default | bit 21) is stricter
/// than asserting "bit 21 is set." The conjunction catches:
///   - silent drop of cap-add (bit 21 is missing → fail),
///   - accidental cap-set replacement (the default bits drop and only
///     SYS_ADMIN remains → fail),
///   - host-side double-add or alias drift (extra bits appear → fail).
#[test]
fn cap_add_sys_admin_sets_bit_21_in_cap_eff() {
    let (ctx, box_id) = run_alpine(&["--cap-add", "SYS_ADMIN"]);
    let _cleanup = BoxCleanup {
        ctx: &ctx,
        id: box_id.clone(),
    };
    let cap_eff = read_cap_eff(&ctx, &box_id);
    let expected = DOCKER_DEFAULT_CAP_EFF | (1u64 << CAP_SYS_ADMIN_BIT);
    assert_eq!(
        cap_eff, expected,
        "CapEff with --cap-add SYS_ADMIN must equal the default mask | bit-21; \
         expected 0x{expected:016x}, got 0x{cap_eff:016x}"
    );
}

/// `--cap-add ALL` must expand to (essentially) every OCI capability.
///
/// Pre-fix (commit 8f63e3b7), the `ALL` branch fell through to the
/// `format!("CAP_{name}")` loop that consumed `capability_names()` —
/// which already returns `CAP_*` strings — yielding `CAP_CAP_*`.
/// Every `Capability::from_str` errored, the container silently kept
/// only the 14-cap default set, and "ALL" became a no-op. This test
/// pins the fix: at least 38 of the 41 OCI cap bits must be set after
/// `--cap-add ALL`. The slack (vs. "exactly 41") accommodates kernel
/// versions where the OCI spec hasn't caught up with a new cap and the
/// kernel reports the cap as unsupported, without giving the test a
/// false-positive on a regression that drops 3+ caps.
#[test]
fn cap_add_all_grants_full_cap_eff() {
    let (ctx, box_id) = run_alpine(&["--cap-add", "ALL"]);
    let _cleanup = BoxCleanup {
        ctx: &ctx,
        id: box_id.clone(),
    };
    let cap_eff = read_cap_eff(&ctx, &box_id);
    let bits = cap_eff.count_ones();
    assert!(
        bits >= 38,
        "--cap-add ALL must set most (>=38) of the 41 OCI cap bits; \
         got {bits} bits set in CapEff = 0x{cap_eff:016x}"
    );
}

//! Integration test: `--cap NAME=0|1` end-to-end.
//!
//! boxlite's cap model defaults *every* Linux capability to on (the VM
//! is the trust boundary, not the container) and exposes `--cap NAME=0`
//! to drop one or `ALL=0` to drop them all. These tests assert on the
//! kernel-visible `CapEff:` mask in `/proc/self/status` — the receiving
//! end of the chain that #597 plumbs:
//!
//!   CLI `--cap NAME=0|1` → BoxOptions.cap_overrides → proto cap_overrides
//!     → guest build_capabilities → OCI Spec process.capabilities
//!     → libcontainer → /proc/self/status::CapEff
//!
//! Asserting at the kernel end catches a regression anywhere in that
//! chain, including ones that pass the CLI parse layer's unit tests.
//! The exec path is the second receiver — `boxlite exec` must see the
//! same cap set as init, not silently revert to a hardcoded default.

use assert_cmd::Command;
use std::time::Duration;

mod common;

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

/// Launch an alpine box (`sleep 600`) with the given extra args, return
/// (ctx, box_id). Caller wraps the id in BoxCleanup so the box is
/// rm-forced even on panic.
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

fn exec_read_cap_eff(ctx: &common::TestContext, box_id: &str) -> u64 {
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

/// Baseline: an unmodified box has *every* OCI cap on.
///
/// The previous "docker-14-cap default" was inverted by the C-design
/// refactor — boxlite's trust boundary is the VM, not the container,
/// so default-restricted-caps was friction without isolation benefit.
/// This test pins the new contract: the empty `cap_overrides` list
/// yields ≥38 of the 41 OCI caps in `CapEff` (slack vs. exactly-41
/// accommodates kernels where the OCI spec hasn't caught up to a
/// new cap).
#[test]
fn default_box_cap_eff_is_essentially_all() {
    let (ctx, box_id) = run_alpine(&[]);
    let _cleanup = BoxCleanup {
        ctx: &ctx,
        id: box_id.clone(),
    };
    let cap_eff = exec_read_cap_eff(&ctx, &box_id);
    let bits = cap_eff.count_ones();
    assert!(
        bits >= 38,
        "default boxlite CapEff must be essentially ALL (>=38 of 41); \
         got {bits} bits set in 0x{cap_eff:016x}. If this fails with the \
         docker 14-cap mask (~0xa80425fb), `default_capabilities()` has \
         crept back into the cap baseline somewhere."
    );
    assert!(
        cap_eff & (1u64 << CAP_SYS_ADMIN_BIT) != 0,
        "default baseline must include CAP_SYS_ADMIN (bit 21); got 0x{cap_eff:016x}"
    );
}

/// `--cap SYS_ADMIN=0` surgically drops one bit, leaves the rest of
/// the default-ALL baseline intact.
///
/// Two-way assertion (bit-21 cleared AND set delta is exactly 1)
/// catches both directions of regression: silent grant (bit-21 still
/// set) and over-broad drop (more than one bit removed).
#[test]
fn cap_drop_sys_admin_clears_bit_21_only() {
    let baseline = {
        let (ctx, id) = run_alpine(&[]);
        let _c = BoxCleanup {
            ctx: &ctx,
            id: id.clone(),
        };
        exec_read_cap_eff(&ctx, &id)
    };

    let (ctx, box_id) = run_alpine(&["--cap", "SYS_ADMIN=0"]);
    let _cleanup = BoxCleanup {
        ctx: &ctx,
        id: box_id.clone(),
    };
    let cap_eff = exec_read_cap_eff(&ctx, &box_id);

    let expected = baseline & !(1u64 << CAP_SYS_ADMIN_BIT);
    assert_eq!(
        cap_eff, expected,
        "--cap SYS_ADMIN=0 must clear only bit 21; \
         baseline = 0x{baseline:016x}, expected = 0x{expected:016x}, \
         got = 0x{cap_eff:016x}"
    );
}

/// `--cap ALL=0` empties the cap set — every bit cleared.
///
/// Functional opposite of the default baseline. A box with zero caps
/// is what an operator picks when treating the container itself as
/// the inner sandbox (e.g. running a CTF-style "all-caps-dropped"
/// payload that should not even be able to bind() a low port).
#[test]
fn cap_all_zero_clears_every_bit() {
    let (ctx, box_id) = run_alpine(&["--cap", "ALL=0"]);
    let _cleanup = BoxCleanup {
        ctx: &ctx,
        id: box_id.clone(),
    };
    let cap_eff = exec_read_cap_eff(&ctx, &box_id);
    assert_eq!(
        cap_eff, 0,
        "--cap ALL=0 must produce empty CapEff; got 0x{cap_eff:016x}"
    );
}

/// Compound: `--cap ALL=0 --cap SYS_ADMIN=1` leaves a 1-bit mask.
///
/// Pins the per-entry ordering rule (later overrides earlier). Without
/// this, an operator who wants "minimum permission + one specific cap"
/// can't express it.
#[test]
fn cap_all_zero_then_sys_admin_one_leaves_just_bit_21() {
    let (ctx, box_id) = run_alpine(&["--cap", "ALL=0", "--cap", "SYS_ADMIN=1"]);
    let _cleanup = BoxCleanup {
        ctx: &ctx,
        id: box_id.clone(),
    };
    let cap_eff = exec_read_cap_eff(&ctx, &box_id);
    assert_eq!(
        cap_eff,
        1u64 << CAP_SYS_ADMIN_BIT,
        "expected CapEff with only bit 21 set; got 0x{cap_eff:016x}"
    );
}

/// `boxlite exec` must inherit the container's cap overrides — not
/// silently fall back to a hardcoded default.
///
/// Pre-refactor, both the TTY and non-TTY exec paths hardcoded their
/// own cap source (TTY: `build_capabilities(&[])`; non-TTY:
/// `capability_names()`). The non-TTY path used a different *list*
/// from the container's init process, so an exec'd `awk` ran with a
/// docker-14-shaped set even when init had SYS_ADMIN. With the
/// default-ALL flip, the same bug would silently re-grant SYS_ADMIN
/// to exec processes after the operator did `--cap SYS_ADMIN=0` —
/// inverted blast radius, same root cause.
///
/// This test pins the symmetric contract: drop SYS_ADMIN at the
/// container level, the bit is also absent in the exec'd process.
#[test]
fn exec_inherits_container_cap_drops() {
    let (ctx, box_id) = run_alpine(&["--cap", "SYS_ADMIN=0"]);
    let _cleanup = BoxCleanup {
        ctx: &ctx,
        id: box_id.clone(),
    };
    let cap_eff = exec_read_cap_eff(&ctx, &box_id);
    assert_eq!(
        cap_eff & (1u64 << CAP_SYS_ADMIN_BIT),
        0,
        "exec must inherit container's --cap SYS_ADMIN=0 drop; \
         exec'd process had bit 21 set in CapEff = 0x{cap_eff:016x}"
    );
}

/// Unknown cap names must fail at `boxlite run` parse, not silently
/// drop into a guest-side warn. Catches the UX regression where a
/// typo like `SYS-ADMIN` (dash) silently no-ops.
#[test]
fn cap_unknown_name_rejected_at_cli_parse() {
    let ctx = common::boxlite();
    let mut run_cmd = ctx.new_cmd();
    run_cmd
        .args([
            "run",
            "-d",
            "--memory",
            "256",
            "--cap",
            "SYS-ADMIN=0", // dash, intentional typo
            "alpine:latest",
            "sleep",
            "30",
        ])
        .timeout(Duration::from_secs(30));
    let out = run_cmd.output().expect("spawn boxlite run");
    assert!(
        !out.status.success(),
        "boxlite run with --cap SYS-ADMIN=0 must error at parse, not run \
         the box silently. stdout = {}, stderr = {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let combined =
        String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    assert!(
        combined.contains("unknown capability"),
        "error message must name the unknown-capability error class; got: {combined}"
    );
}

//! Guest-side version-compatibility guard on `sys_mount_options`.
//!
//! `sys_mount_options` is a host-resolved literal OCI value, carried by the
//! caller and applied verbatim — nothing here re-derives it (see
//! docs/architecture/privileged-mode-design.md, Trade-offs, option F).
//! `readonly_paths` rides along on the same guarantee this guard checks (see
//! [`validate_sys_mount_options`]) but has no logic of its own here.
//! `capabilities` is a separate concern entirely, resolved against the
//! guest's own kernel by [`super::capabilities::CapabilitySet::resolve`].

use boxlite_shared::errors::{BoxliteError, BoxliteResult};

/// Reject an empty `sys_mount_options` as a host/guest version mismatch.
///
/// `sys_mount_options` has no legitimate empty value: every real host
/// resolves at least `["rbind", "nosuid", "noexec", "nodev"]`, privileged or
/// not (advanced_options::sys_mount_options). Empty here can only mean the
/// host predates this field entirely — a version too old to know it should
/// send anything — not a deliberate request. Rejecting it here also means
/// `readonly_paths` being empty is trustworthy as the real "privileged"
/// signal whenever this check passes: an old host that doesn't know about
/// `sys_mount_options` doesn't know about `readonly_paths` either, so the two
/// are never legitimately split.
///
/// A real boot test confirmed the alternative (silently proceeding) doesn't
/// even degrade gracefully: youki's mount code requires `bind`/`rbind` in
/// `options` to treat `/sys` as a bind mount at all, so an empty list makes
/// container creation fail with an unrelated "failed to prepare rootfs" error
/// — this rejection turns that into a diagnosable one.
pub(crate) fn validate_sys_mount_options(sys_mount_options: &[String]) -> BoxliteResult<()> {
    if sys_mount_options.is_empty() {
        return Err(BoxliteError::Unsupported(
            "advanced.sys_mount_options is empty; the host predates \
             resolved security fields (privileged-mode-design.md, option F) \
             — recreate this box with a matching boxlite version"
                .to_string(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_sys_mount_options_is_rejected_as_a_version_mismatch() {
        let error =
            validate_sys_mount_options(&[]).expect_err("empty sys_mount_options must not pass");

        assert!(matches!(error, BoxliteError::Unsupported(_)), "{error:?}");
        assert!(
            error.to_string().contains("sys_mount_options"),
            "error should name the field: {error}"
        );
    }

    #[test]
    fn non_empty_sys_mount_options_passes() {
        validate_sys_mount_options(&["rbind".to_string()])
            .expect("non-empty sys_mount_options should pass");
    }
}

//! Guest-side version-compatibility guard on `advanced.mount`.
//!
//! `options` is a host-resolved literal OCI value, carried by the caller and
//! applied verbatim — nothing here re-derives it (see
//! docs/architecture/privileged-mode-design.md, Trade-offs, option F).
//! `destination` names which of the guest's own standard mounts that value
//! is for; this guest only knows how to apply it to `/sys`. `readonly_paths`
//! rides along on the same guarantee this guard checks (see
//! [`validate_sys_mount_options`]) but has no logic of its own here.
//! `capabilities` is a separate concern entirely, resolved against the
//! guest's own kernel by [`super::capabilities::CapabilitySet::resolve`].

use boxlite_shared::errors::{BoxliteError, BoxliteResult};

/// Reject a `destination` this guest can't apply, or an `options` list that
/// signals a host/guest version mismatch.
///
/// `destination` is data, not a hardcoded assumption, so a host newer than
/// this guest could legitimately name a mount this guest has no override
/// logic for yet; failing clearly here beats silently applying `options` to
/// the wrong mount or dropping it.
///
/// `options` has no legitimate empty value for `/sys`: every real host
/// resolves at least `["rbind", "nosuid", "noexec", "nodev"]`, privileged or
/// not (advanced_options::sys_mount_options). Empty here can only mean the
/// host predates this field entirely — a version too old to know it should
/// send anything — not a deliberate request. Rejecting it here also means
/// `readonly_paths` being empty is trustworthy as the real "privileged"
/// signal whenever this check passes: an old host that doesn't know about
/// `options` doesn't know about `readonly_paths` either, so the two are never
/// legitimately split.
///
/// A real boot test confirmed the alternative (silently proceeding) doesn't
/// even degrade gracefully: youki's mount code requires `bind`/`rbind` in
/// `options` to treat `/sys` as a bind mount at all, so an empty list makes
/// container creation fail with an unrelated "failed to prepare rootfs" error
/// — this rejection turns that into a diagnosable one.
pub(crate) fn validate_sys_mount_options(
    destination: &str,
    options: &[String],
) -> BoxliteResult<()> {
    if destination != "/sys" {
        return Err(BoxliteError::Unsupported(format!(
            "advanced.mount.destination {destination:?} is not supported; this guest \
             only knows how to apply host-resolved options to /sys"
        )));
    }
    if options.is_empty() {
        return Err(BoxliteError::Unsupported(
            "advanced.mount.options is empty; the host predates \
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
    fn empty_options_is_rejected_as_a_version_mismatch() {
        let error =
            validate_sys_mount_options("/sys", &[]).expect_err("empty options must not pass");

        assert!(matches!(error, BoxliteError::Unsupported(_)), "{error:?}");
        assert!(
            error.to_string().contains("options"),
            "error should name the field: {error}"
        );
    }

    #[test]
    fn non_empty_options_for_sys_passes() {
        validate_sys_mount_options("/sys", &["rbind".to_string()])
            .expect("non-empty options for /sys should pass");
    }

    #[test]
    fn unsupported_destination_is_rejected() {
        let error = validate_sys_mount_options("/proc", &["rbind".to_string()])
            .expect_err("a destination other than /sys must not pass");

        assert!(matches!(error, BoxliteError::Unsupported(_)), "{error:?}");
        assert!(
            error.to_string().contains("/proc"),
            "error should name the unsupported destination: {error}"
        );
    }
}

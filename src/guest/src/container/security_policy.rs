//! The guest's resolved security policy for one container.
//!
//! Bundles the atomic security choices for a container: two host-resolved
//! literal OCI values (`readonly_paths`, `sys_mount_options`) alongside the
//! one thing the guest resolves itself (`capabilities`, against its own
//! kernel — the one thing the host cannot know across the VM boundary). See
//! docs/architecture/privileged-mode-design.md, Trade-offs, option F.

use super::capabilities::CapabilitySet;
use boxlite_shared::errors::{BoxliteError, BoxliteResult};
use boxlite_shared::ContainerCapabilities;

/// The guest's resolved security policy for one container.
///
/// The host has already resolved high-level security semantics into the
/// literal OCI values below — `readonly_paths`/`sys_mount_options` are
/// applied verbatim, never re-derived here (see
/// docs/architecture/privileged-mode-design.md, Trade-offs, option F). No
/// `masked_paths`: nothing in the DinD workflow reads a masked path, so the
/// guest keeps applying its own oci-spec default unconditionally, unaffected
/// by `privileged`. The guest resolves only `capabilities`: canonical
/// add/drop names against its own kernel, the one thing the host cannot know
/// across the VM boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ResolvedSecurityPolicy {
    pub(crate) capabilities: CapabilitySet,
    pub(crate) readonly_paths: Vec<String>,
    pub(crate) sys_mount_options: Vec<String>,
}

impl ResolvedSecurityPolicy {
    pub(crate) fn from_resolved(
        policy: ContainerCapabilities,
        readonly_paths: Vec<String>,
        sys_mount_options: Vec<String>,
    ) -> BoxliteResult<Self> {
        // `sys_mount_options` has no legitimate empty value: every real host
        // resolves at least `["rbind", "nosuid", "noexec", "nodev"]`, privileged
        // or not (advanced_options::sys_mount_options). Empty here can only mean
        // the host predates this field entirely — a version too old to know it
        // should send anything — not a deliberate request. Rejecting it here
        // also means `readonly_paths` being empty is trustworthy as the real
        // "privileged" signal whenever this check passes: an old host that
        // doesn't know about `sys_mount_options` doesn't know about
        // `readonly_paths` either, so the two are never legitimately split.
        if sys_mount_options.is_empty() {
            return Err(BoxliteError::Unsupported(
                "advanced.sys_mount_options is empty; the host predates \
                 resolved security fields (privileged-mode-design.md, option F) \
                 — recreate this box with a matching boxlite version"
                    .to_string(),
            ));
        }

        Ok(Self {
            capabilities: CapabilitySet::resolve(&policy.add, &policy.drop)?,
            readonly_paths,
            sys_mount_options,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oci_spec::runtime::Capability;

    #[test]
    fn resolved_policy_carries_host_resolved_paths_verbatim() {
        let policy = ResolvedSecurityPolicy::from_resolved(
            ContainerCapabilities {
                add: vec!["ALL".into()],
                ..Default::default()
            },
            Vec::new(),
            vec![
                "rbind".into(),
                "nosuid".into(),
                "noexec".into(),
                "nodev".into(),
            ],
        )
        .expect("security policy should resolve");

        assert!(policy.readonly_paths.is_empty());
        assert!(!policy.sys_mount_options.contains(&"rro".to_string()));
        assert!(policy.capabilities.contains(&Capability::SysAdmin));
        assert!(policy.capabilities.contains(&Capability::NetRaw));
    }

    /// `sys_mount_options` is never legitimately empty (every real host
    /// resolves at least the 4 base flags). An empty list can only mean the
    /// host predates these fields entirely — reject it instead of silently
    /// trusting `readonly_paths` as a deliberate "privileged" request. A real
    /// boot test confirmed the alternative (silently proceeding) doesn't even
    /// degrade gracefully: youki's mount code requires `bind`/`rbind` in
    /// `options` to treat `/sys` as a bind mount at all, so an empty list
    /// makes container creation fail with an unrelated "failed to prepare
    /// rootfs" error — this rejection turns that into a diagnosable one.
    #[test]
    fn empty_sys_mount_options_is_rejected_as_a_version_mismatch() {
        let error = ResolvedSecurityPolicy::from_resolved(
            ContainerCapabilities::default(),
            Vec::new(),
            Vec::new(),
        )
        .expect_err("empty sys_mount_options must not resolve");

        assert!(matches!(error, BoxliteError::Unsupported(_)), "{error:?}");
        assert!(
            error.to_string().contains("sys_mount_options"),
            "error should name the field: {error}"
        );
    }

    /// Capabilities and OCI paths are separate knobs the host resolves
    /// independently: capability escalation (`ALL`) does not imply the host
    /// also cleared readonly paths — the guest applies whatever it was sent,
    /// verbatim, with no re-derivation from the capability policy.
    #[test]
    fn all_capabilities_with_hardened_paths_keeps_them_readonly() {
        let readonly_paths = vec!["/proc/sys".to_string()];
        let policy = ResolvedSecurityPolicy::from_resolved(
            ContainerCapabilities {
                add: vec!["ALL".into()],
                ..Default::default()
            },
            readonly_paths.clone(),
            vec![
                "rbind".into(),
                "nosuid".into(),
                "noexec".into(),
                "nodev".into(),
                "rro".into(),
            ],
        )
        .expect("capability-only policy should resolve");

        assert_eq!(policy.readonly_paths, readonly_paths);
        assert!(policy.capabilities.contains(&Capability::SysAdmin));
    }
}

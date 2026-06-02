//! Linux capabilities for container processes.
//!
//! Defines the default capability set matching Docker/OCI defaults.
//! Used by:
//! - OCI spec builder (process.capabilities)
//! - Tenant process spawning (exec capabilities)

use oci_spec::runtime::Capability;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// Guest-local mirror of `boxlite_shared::CapOverride` that derives
/// serde so it can travel through the zygote's `BuildSpec` IPC.
///
/// Kept separate from the prost-generated proto type to avoid
/// sprinkling `#[derive(serde::Serialize, serde::Deserialize)]` over
/// the proto build config. Conversion happens at the gRPC boundary
/// (`service::container`) in one spot.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct CapOverride {
    /// Capability name (`SYS_ADMIN`, optionally `CAP_`-prefixed; or
    /// the reserved literal `ALL`).
    pub name: String,
    /// true → cap kept/granted; false → cap dropped.
    pub enabled: bool,
}

impl From<boxlite_shared::CapOverride> for CapOverride {
    fn from(proto: boxlite_shared::CapOverride) -> Self {
        Self {
            name: proto.name,
            enabled: proto.enabled,
        }
    }
}

/// Every Linux capability known to the OCI spec — the boxlite default
/// baseline (the VM is the trust boundary, not the container).
/// `--cap NAME=0` drops one; `--cap ALL=0` drops them all.
pub fn all_capabilities() -> HashSet<Capability> {
    [
        Capability::AuditControl,
        Capability::AuditRead,
        Capability::AuditWrite,
        Capability::BlockSuspend,
        Capability::Bpf,
        Capability::CheckpointRestore,
        Capability::Chown,
        Capability::DacOverride,
        Capability::DacReadSearch,
        Capability::Fowner,
        Capability::Fsetid,
        Capability::IpcLock,
        Capability::IpcOwner,
        Capability::Kill,
        Capability::Lease,
        Capability::LinuxImmutable,
        Capability::MacAdmin,
        Capability::MacOverride,
        Capability::Mknod,
        Capability::NetAdmin,
        Capability::NetBindService,
        Capability::NetBroadcast,
        Capability::NetRaw,
        Capability::Perfmon,
        Capability::Setgid,
        Capability::Setfcap,
        Capability::Setpcap,
        Capability::Setuid,
        Capability::SysAdmin,
        Capability::SysBoot,
        Capability::SysChroot,
        Capability::SysModule,
        Capability::SysNice,
        Capability::SysPacct,
        Capability::SysPtrace,
        Capability::SysRawio,
        Capability::SysResource,
        Capability::SysTime,
        Capability::SysTtyConfig,
        Capability::Syslog,
        Capability::WakeAlarm,
    ]
    .into_iter()
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_capabilities_includes_each_dangerous_cap() {
        // Pins the default-ALL contract from the boxlite side: the cap
        // set used as the resolver baseline must actually include the
        // dangerous-cap rows the operator would otherwise need to
        // `--cap-add` explicitly under a docker-style model.
        let caps = all_capabilities();
        for cap in [
            Capability::SysAdmin,
            Capability::NetAdmin,
            Capability::SysModule,
            Capability::SysRawio,
            Capability::SysPtrace,
            Capability::Bpf,
        ] {
            assert!(
                caps.contains(&cap),
                "default-ALL baseline must include {cap:?}; got {caps:?}"
            );
        }
    }

    #[test]
    fn cap_override_proto_conversion_preserves_fields() {
        let proto = boxlite_shared::CapOverride {
            name: "SYS_ADMIN".to_string(),
            enabled: false,
        };
        let local: CapOverride = proto.into();
        assert_eq!(local.name, "SYS_ADMIN");
        assert!(!local.enabled);
    }
}

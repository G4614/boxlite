//! Constants for BoxLite runtime
//!
//! Centralized location for all hardcoded values, paths, and configuration.
//! Host controls all paths - guest receives these via GuestInitRequest.

// Re-export shared constants from boxlite-core
pub use boxlite_shared::constants::{container, mount_tags, network};

/// Guest mount points (paths inside the guest).
///
/// Note: Host only knows BIN_DIR (for guest entrypoint).
/// All other guest paths are determined by the guest based on tags.
pub mod guest_paths {
    /// Guest binary directory (for guest entrypoint executable)
    pub const BIN_DIR: &str = "/boxlite/bin";
}

pub mod envs {
    pub const BOXLITE_HOME: &str = "BOXLITE_HOME";

    /// REST API base URL (required for REST mode).
    #[cfg(feature = "rest")]
    pub const BOXLITE_REST_URL: &str = "BOXLITE_REST_URL";

    /// Opaque API key, sent directly as `Authorization: Bearer <key>`. Flat
    /// name (not `BOXLITE_REST_API_KEY`) matches industry convention —
    /// `STRIPE_API_KEY`, `HEROKU_API_KEY`, `GH_TOKEN`.
    #[cfg(feature = "rest")]
    pub const BOXLITE_API_KEY: &str = "BOXLITE_API_KEY";

    /// Value substituted into the `{prefix}` URL segment on
    /// box-scoped routes (`/v1/{prefix}/boxes/...`). Opaque
    /// to the client — deployment decides what it means. When
    /// unset / empty the client builds URLs without the segment
    /// (`/v1/boxes/...`) — the canonical single-tenant shape
    /// used by `boxlite serve` and similar single-scope deployments.
    #[cfg(feature = "rest")]
    pub const BOXLITE_REST_PATH_PREFIX: &str = "BOXLITE_REST_PATH_PREFIX";
}

/// Container images used by the runtime
pub mod images {
    /// Default container image when none is specified
    pub const DEFAULT: &str = "alpine:latest";

    /// Base image for VM init rootfs (must include mkfs.ext4 for disk formatting)
    pub const INIT_ROOTFS: &str = "debian:bookworm-slim";
}

/// Filesystem and mount options
pub mod fs_options {
    /// Default tmpfs size for writable layer (in MB)
    pub const TMPFS_SIZE_MB: usize = 1024;

    /// Overlayfs mount options
    pub const OVERLAYFS_OPTIONS: &[&str] =
        &["metacopy=off", "redirect_dir=off", "index=off", "xino=off"];
}

/// Virtual machine resource defaults
pub mod vm_defaults {
    /// Default number of CPUs allocated to a Box
    pub const DEFAULT_CPUS: u8 = 1;

    /// Default memory in MiB allocated to a Box
    pub const DEFAULT_MEMORY_MIB: u32 = 2048;

    /// Default disk size in GB for the container rootfs (sparse, grows as needed)
    pub const DEFAULT_DISK_SIZE_GB: u64 = 10;
}

/// Host disk-space admission guard for box startup.
///
/// A box can grow the host filesystem via its COW overlay and (unbounded)
/// virtio-fs volume writes. There is no runtime quota, so this is admission
/// control: refuse to start when the host is critically low, warn when it is
/// getting low. It does not stop a running box from filling the disk.
pub mod disk_guard {
    /// Below this much free space, refuse to start a box. At <1 GiB any disk
    /// write (qcow2 growth, image extraction) is liable to fail mid-operation.
    pub const MIN_FREE_BYTES_HARD: u64 = 1024 * 1024 * 1024;

    /// Below this much free space, warn but proceed.
    pub const MIN_FREE_BYTES_SOFT: u64 = 5 * 1024 * 1024 * 1024;

    /// Below this fraction of total capacity free, warn but proceed. Catches
    /// "low" on small filesystems where 5 GiB is most of the disk. Percentage
    /// is intentionally warn-only — a large disk at 9% free still has plenty.
    pub const MIN_FREE_FRACTION_SOFT: f64 = 0.10;
}

/// File naming patterns
pub mod filenames {
    use crate::runtime::layout::dirs;
    use std::path::{Path, PathBuf};

    /// Lock file name
    pub const LOCK_FILE: &str = ".lock";

    pub fn box_home(home_dir: &Path, box_id: &str) -> PathBuf {
        home_dir.join(dirs::BOXES_DIR).join(box_id)
    }

    /// Get full path for Unix socket
    pub fn unix_socket_path(home_dir: &Path, box_id: &str) -> PathBuf {
        box_home(home_dir, box_id)
            .join(dirs::SOCKETS_DIR)
            .join("box.sock")
    }
}

//! Box import from `.boxlite` archives.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use boxlite_shared::errors::{BoxliteError, BoxliteResult};

use crate::disk::constants::filenames as disk_filenames;
use crate::litebox::LiteBox;
use crate::litebox::archive::{
    ArchiveLayer, ArchiveManifest, MANIFEST_FILENAME, MAX_SUPPORTED_VERSION,
    PUBLISHED_PORTS_ARCHIVE_VERSION, extract_archive, layer_entry_name, move_file, sha256_file,
};
use crate::runtime::advanced_options::SecurityOptions;
use crate::runtime::id::BaseDiskID;
use crate::runtime::options::{
    ArchiveImportPolicy, BoxArchive, BoxOptions, RootfsSpec, normalize_legacy_ports,
};
use crate::runtime::rt_impl::RuntimeImpl;
use crate::runtime::types::BoxStatus;

/// Import a box from a `.boxlite` archive.
///
/// Creates a new box with a new ID from archived disk images and
/// configuration. The imported box starts in `Stopped` state.
pub(crate) async fn import_box(
    runtime: &Arc<RuntimeImpl>,
    archive: BoxArchive,
    name: Option<String>,
) -> BoxliteResult<LiteBox> {
    let t0 = std::time::Instant::now();
    let archive_path = archive.path().to_path_buf();
    if !archive_path.exists() {
        return Err(BoxliteError::NotFound(format!(
            "Archive not found: {}",
            archive_path.display()
        )));
    }

    // Phase 1: Extract and validate archive (blocking I/O).
    let layout = runtime.layout.clone();
    let (manifest, temp_dir) =
        tokio::task::spawn_blocking(move || extract_and_validate(&archive_path, &layout))
            .await
            .map_err(|e| {
                BoxliteError::Internal(format!("Import extraction task panicked: {}", e))
            })??;

    let options = options_from_manifest(&manifest, archive.import_policy())?;

    // Phase 2: Validate disks and install into a staging directory (blocking I/O).
    // The staging dir lives inside temp_dir; provision_box will rename it.
    let staging_dir = temp_dir.path().join("staging");
    let temp_path = temp_dir.path().to_path_buf();
    let staging_clone = staging_dir.clone();
    let layers = manifest.layers.clone();
    let base_disk_mgr = runtime.base_disk_mgr.clone();
    let installed = tokio::task::spawn_blocking(move || {
        if layers.is_empty() {
            install_disks(&temp_path, &staging_clone).map(|()| Vec::new())
        } else {
            install_layers(&layers, &temp_path, &staging_clone, &base_disk_mgr)
        }
    })
    .await
    .map_err(|e| BoxliteError::Internal(format!("Import install task panicked: {}", e)))??;

    let litebox = runtime
        .provision_box(staging_dir, name, options, BoxStatus::Stopped)
        .await?;

    // Keep every base the imported box now reads through alive: the GC drops a
    // base once no box references it, and this box is the only reference a
    // freshly materialized layer has.
    for base_id in &installed {
        if let Err(e) = runtime
            .base_disk_mgr
            .store()
            .add_ref(base_id, litebox.id().as_ref())
        {
            tracing::warn!(
                box_id = %litebox.id(),
                base_disk_id = %base_id,
                error = %e,
                "Failed to record base disk ref for imported box"
            );
        }
    }

    tracing::info!(
        box_id = %litebox.id(),
        elapsed_ms = t0.elapsed().as_millis() as u64,
        "Imported box from archive"
    );

    Ok(litebox)
}

/// Read the persisted configuration, falling back to the v1/v2 image field.
///
/// An archive is untrusted input, so its options are validated here rather
/// than after disks have been installed and box metadata persisted.
fn options_from_manifest(
    manifest: &ArchiveManifest,
    policy: ArchiveImportPolicy,
) -> BoxliteResult<BoxOptions> {
    let mut options = manifest.box_options.clone().unwrap_or_else(|| BoxOptions {
        rootfs: RootfsSpec::Image(manifest.image.clone()),
        ..Default::default()
    });
    // Up to v4 an archive's ports carried no publication semantics: a null
    // host port meant the guest port, and host_ip and protocol were ignored.
    // Canonicalize before sanitize, so the rewritten mappings are validated.
    if manifest.version < PUBLISHED_PORTS_ARCHIVE_VERSION {
        let changed_mappings = normalize_legacy_ports(&mut options.ports);
        if changed_mappings > 0 {
            tracing::warn!(
                archive_version = manifest.version,
                changed_mappings,
                "Canonicalized legacy archive port mappings"
            );
        }
    }
    options.sanitize().map_err(|error| {
        BoxliteError::InvalidArgument(format!("invalid archive box_options: {error}"))
    })?;

    if policy == ArchiveImportPolicy::Trusted {
        return Ok(options);
    }

    // An upload must not reach into the server's host or pick its own
    // isolation, so refuse everything that would and impose server defaults.
    if options.advanced.kernel.is_some() {
        return Err(rejected_upload("custom kernels"));
    }
    if options.advanced.nested_virtualization {
        return Err(rejected_upload("nested virtualization"));
    }
    if matches!(options.rootfs, RootfsSpec::RootfsPath(_)) {
        return Err(rejected_upload("host rootfs paths"));
    }
    if !options.volumes.is_empty() {
        return Err(rejected_upload("host volume mounts"));
    }
    options.advanced.security = SecurityOptions::default();

    Ok(options)
}

fn rejected_upload(subject: &str) -> BoxliteError {
    BoxliteError::Unsupported(format!(
        "{subject} cannot be requested by an archive uploaded through a REST server"
    ))
}

/// Extract archive, parse manifest, verify checksums.
fn extract_and_validate(
    archive_path: &Path,
    layout: &crate::runtime::layout::FilesystemLayout,
) -> BoxliteResult<(ArchiveManifest, tempfile::TempDir)> {
    let temp_dir = tempfile::tempdir_in(layout.temp_dir())
        .map_err(|e| BoxliteError::Storage(format!("Failed to create temp directory: {}", e)))?;

    extract_archive(archive_path, temp_dir.path())?;

    let manifest_path = temp_dir.path().join(MANIFEST_FILENAME);
    if !manifest_path.exists() {
        return Err(BoxliteError::Storage(
            "Invalid archive: manifest.json not found".to_string(),
        ));
    }

    let manifest_json = std::fs::read_to_string(&manifest_path)?;
    let manifest: ArchiveManifest = serde_json::from_str(&manifest_json)
        .map_err(|e| BoxliteError::Storage(format!("Invalid manifest: {}", e)))?;

    if manifest.version > MAX_SUPPORTED_VERSION {
        return Err(BoxliteError::Storage(format!(
            "Unsupported archive version {} (max supported: {}). Upgrade boxlite.",
            manifest.version, MAX_SUPPORTED_VERSION
        )));
    }

    // A layered archive carries `layers/` blobs instead of a flattened disk;
    // each is checked against its own digest as it is installed.
    if !manifest.layers.is_empty() {
        return Ok((manifest, temp_dir));
    }

    let extracted_container = temp_dir.path().join(disk_filenames::CONTAINER_DISK);
    if !extracted_container.exists() {
        return Err(BoxliteError::Storage(format!(
            "Invalid archive: {} not found",
            disk_filenames::CONTAINER_DISK
        )));
    }

    // Verify checksums (v2+ archives have non-empty checksums).
    if !manifest.container_disk_checksum.is_empty() {
        let actual = sha256_file(&extracted_container)?;
        if actual != manifest.container_disk_checksum {
            return Err(BoxliteError::Storage(format!(
                "Container disk checksum mismatch: expected {}, got {}",
                manifest.container_disk_checksum, actual
            )));
        }
    }

    // A guest rootfs disk carried by an older archive is ignored, so it is
    // neither checksummed nor installed — see `install_disks`.

    Ok((manifest, temp_dir))
}

/// Materialize a layered archive's chain and relink it, returning the ids of
/// the base disks the imported box now depends on.
///
/// A layer already present locally — same content digest — is reused as-is and
/// its blob is never written, which is where cross-box dedup comes from: every
/// box built from an image shares that image's layer.
///
/// Security: the manifest carries digests, never paths. Each child is relinked
/// to a path *this* function chose and canonicalized locally, and the resulting
/// header is read back and checked, so a crafted archive cannot aim a backing
/// file at a host path of its choosing. Every blob is verified against its
/// declared digest before anything points at it.
fn install_layers(
    layers: &[ArchiveLayer],
    temp_dir: &Path,
    box_home: &Path,
    base_disk_mgr: &crate::disk::BaseDiskManager,
) -> BoxliteResult<Vec<BaseDiskID>> {
    let Some((top, bases)) = layers.split_last() else {
        return Err(BoxliteError::Storage(
            "Invalid archive: layered manifest has no layers".to_string(),
        ));
    };

    let disks_dir = box_home.join("disks");
    std::fs::create_dir_all(&disks_dir).map_err(|e| {
        BoxliteError::Storage(format!(
            "Failed to create disks directory {}: {}",
            disks_dir.display(),
            e
        ))
    })?;

    // Materialize the bases bottom-up, so each layer's parent already exists
    // by the time it is relinked.
    let mut base_ids = Vec::new();
    let mut parent: Option<PathBuf> = None;
    for layer in bases {
        let (path, id) = resolve_layer(layer, temp_dir, base_disk_mgr)?;
        if let Some(id) = id {
            base_ids.push(id);
        }
        if let Some(parent_path) = &parent {
            relink(&path, parent_path)?;
        }
        parent = Some(path);
    }

    // The top layer is the box's own container disk.
    let container = disks_dir.join(disk_filenames::CONTAINER_DISK);
    let blob = extracted_layer_path(temp_dir, top);
    verify_layer_digest(&blob, &top.digest)?;
    move_file(&blob, &container)?;

    match &parent {
        Some(parent_path) => relink(&container, parent_path)?,
        // A single-layer chain stands alone, so it must not reference anything.
        None => validate_no_backing_references(&container)?,
    }

    Ok(base_ids)
}

/// Path a layer blob was extracted to.
fn extracted_layer_path(temp_dir: &Path, layer: &ArchiveLayer) -> PathBuf {
    temp_dir.join(layer_entry_name(&layer.digest))
}

/// Fail unless a blob hashes to the digest the manifest declared for it.
fn verify_layer_digest(path: &Path, digest: &str) -> BoxliteResult<()> {
    if !path.exists() {
        return Err(BoxliteError::Storage(format!(
            "Invalid archive: layer {digest} is missing from the archive"
        )));
    }
    let actual = sha256_file(path)?;
    if actual != digest {
        return Err(BoxliteError::Storage(format!(
            "Layer digest mismatch: expected {digest}, got {actual}"
        )));
    }
    Ok(())
}

/// Return where a layer lives locally, installing it if this host lacks it.
///
/// The returned id is `Some` only when a base disk record exists to reference,
/// which is what keeps a newly installed layer from being garbage-collected.
fn resolve_layer(
    layer: &ArchiveLayer,
    temp_dir: &Path,
    base_disk_mgr: &crate::disk::BaseDiskManager,
) -> BoxliteResult<(PathBuf, Option<BaseDiskID>)> {
    if let Some(existing) = base_disk_mgr.store().find_by_digest(&layer.digest)? {
        let path = PathBuf::from(&existing.disk.disk_info.base_path);
        if path.exists() {
            tracing::debug!(digest = %layer.digest, "Layer already present, skipping transfer");
            return Ok((path, Some(existing.disk.id)));
        }
        // The record outlived its file; fall through and reinstall the blob.
    }

    let blob = extracted_layer_path(temp_dir, layer);
    verify_layer_digest(&blob, &layer.digest)?;
    let installed = base_disk_mgr.install_layer(&blob, &layer.digest)?;
    Ok((installed.disk_info.to_path_buf(), Some(installed.id)))
}

/// Point a child qcow2 at a parent path chosen by this host, then prove it took.
fn relink(child: &Path, parent: &Path) -> BoxliteResult<()> {
    crate::disk::set_backing_file_path(child, parent)?;

    let expected = parent
        .canonicalize()
        .unwrap_or_else(|_| parent.to_path_buf());
    match crate::disk::read_backing_file_path(child)? {
        Some(actual) if Path::new(&actual) == expected => Ok(()),
        other => Err(BoxliteError::InvalidState(format!(
            "Refusing imported disk '{}': backing file is {:?} after relink, expected {}",
            child.display(),
            other,
            expected.display()
        ))),
    }
}

/// Validate disk security and move the container disk into box_home/disks/.
///
/// The guest rootfs disk is never installed, even when an older archive carries
/// one. It holds no user state, and letting an archived copy win would bypass
/// the importing host's own version-keyed guest rootfs cache: export flattens
/// the overlay, so the archived disk has no backing reference and
/// `validate_reusable_guest_rootfs_disk` would accept it verbatim. Leaving it
/// absent makes the next start rebuild the overlay from the local cache, which
/// is what clone and snapshot-restore already do.
fn install_disks(temp_dir: &Path, box_home: &Path) -> BoxliteResult<()> {
    // Security: Reject imported disks that reference backing files.
    // A crafted archive could include a qcow2 with a backing reference to
    // /etc/shadow or another box's disk, leaking data on first read.
    let extracted_container = temp_dir.join(disk_filenames::CONTAINER_DISK);
    validate_no_backing_references(&extracted_container)?;

    let disks_dir = box_home.join("disks");
    std::fs::create_dir_all(&disks_dir).map_err(|e| {
        BoxliteError::Storage(format!(
            "Failed to create disks directory {}: {}",
            disks_dir.display(),
            e
        ))
    })?;

    move_file(
        &extracted_container,
        &disks_dir.join(disk_filenames::CONTAINER_DISK),
    )?;

    Ok(())
}

/// Reject qcow2 disks with backing file references (security check).
pub(crate) fn validate_no_backing_references(disk_path: &Path) -> BoxliteResult<()> {
    if let Ok(Some(backing)) = crate::disk::read_backing_file_path(disk_path) {
        return Err(BoxliteError::InvalidState(format!(
            "Imported disk '{}' has backing file reference '{}'. \
             This is not allowed for security reasons.",
            disk_path.display(),
            backing
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn v3_manifest(options: BoxOptions) -> ArchiveManifest {
        ArchiveManifest {
            version: 3,
            box_name: None,
            image: "alpine:latest".to_string(),
            box_options: Some(options),
            guest_disk_checksum: String::new(),
            container_disk_checksum: String::new(),
            layers: Vec::new(),
            exported_at: "2026-07-26T00:00:00Z".to_string(),
        }
    }

    fn loopback_port() -> crate::runtime::options::PortSpec {
        crate::runtime::options::PortSpec {
            host_port: Some(18080),
            guest_port: 80,
            protocol: crate::runtime::options::PortProtocol::Tcp,
            host_ip: Some("127.0.0.1".to_string()),
        }
    }

    /// The importer's canonicalization window is the other half of the archive
    /// version contract: below v5 a mapping predates publication semantics and
    /// must be rewritten, at v5 it carries them and must be left exactly alone.
    /// A window that swallowed v5 would clear `host_ip` and turn a loopback
    /// publication into one on every interface.
    #[test]
    fn canonicalization_window_stops_at_the_published_ports_version() {
        let options = BoxOptions {
            ports: vec![loopback_port()],
            ..Default::default()
        };

        let mut legacy = v3_manifest(options.clone());
        legacy.version = PUBLISHED_PORTS_ARCHIVE_VERSION - 1;
        let rewritten = options_from_manifest(&legacy, ArchiveImportPolicy::Trusted).unwrap();
        assert_eq!(
            rewritten.ports[0].host_ip, None,
            "a pre-publication archive never meant its bind IP"
        );

        let mut current = v3_manifest(options.clone());
        current.version = PUBLISHED_PORTS_ARCHIVE_VERSION;
        let preserved = options_from_manifest(&current, ArchiveImportPolicy::Trusted).unwrap();
        assert_eq!(
            preserved.ports, options.ports,
            "a v5 archive carries publication semantics and must survive import intact"
        );
    }

    #[test]
    fn untrusted_import_rejects_nested_virtualization() {
        let options = BoxOptions {
            advanced: crate::runtime::advanced_options::AdvancedBoxOptions {
                nested_virtualization: true,
                ..Default::default()
            },
            ..Default::default()
        };

        let error =
            options_from_manifest(&v3_manifest(options), ArchiveImportPolicy::UntrustedRemote)
                .unwrap_err();

        assert!(matches!(error, BoxliteError::Unsupported(_)), "{error:?}");
        assert!(error.to_string().contains("nested virtualization"));
    }

    #[test]
    fn untrusted_import_rejects_custom_kernel() {
        // A real file, so `sanitize()` passes and the upload policy — not path
        // validation — is what rejects the archive.
        let kernel = tempfile::NamedTempFile::new().unwrap();
        let mut options = BoxOptions::default();
        options.advanced.kernel = Some(crate::experimental::custom_kernel::KernelOptions::new(
            kernel.path(),
        ));

        let error =
            options_from_manifest(&v3_manifest(options), ArchiveImportPolicy::UntrustedRemote)
                .unwrap_err();

        assert!(matches!(error, BoxliteError::Unsupported(_)), "{error:?}");
        assert!(error.to_string().contains("custom kernels"));
    }

    #[test]
    fn untrusted_import_rejects_host_volumes() {
        let mut options = BoxOptions::default();
        options.volumes.push(crate::runtime::options::VolumeSpec {
            host_path: "/".to_string(),
            guest_path: "/host".to_string(),
            read_only: false,
        });

        let error =
            options_from_manifest(&v3_manifest(options), ArchiveImportPolicy::UntrustedRemote)
                .expect_err("untrusted archives must not select server host paths");

        assert!(matches!(error, BoxliteError::Unsupported(_)), "{error:?}");
        assert!(error.to_string().contains("host volume mounts"));
    }

    #[test]
    fn untrusted_import_rejects_host_rootfs_paths() {
        let options = BoxOptions {
            rootfs: RootfsSpec::RootfsPath("/".to_string()),
            ..Default::default()
        };

        let error =
            options_from_manifest(&v3_manifest(options), ArchiveImportPolicy::UntrustedRemote)
                .expect_err("untrusted archives must not select a server rootfs path");

        assert!(matches!(error, BoxliteError::Unsupported(_)), "{error:?}");
        assert!(error.to_string().contains("host rootfs paths"));
    }

    #[test]
    fn untrusted_import_replaces_archive_security_with_server_default() {
        let mut options = BoxOptions::default();
        options.advanced.security = SecurityOptions::disabled();

        let resolved =
            options_from_manifest(&v3_manifest(options), ArchiveImportPolicy::UntrustedRemote)
                .unwrap();

        assert_eq!(resolved.advanced.security, SecurityOptions::default());
    }

    #[test]
    fn trusted_import_preserves_archive_configuration() {
        let mut options = BoxOptions {
            advanced: crate::runtime::advanced_options::AdvancedBoxOptions {
                nested_virtualization: true,
                ..Default::default()
            },
            ..Default::default()
        };
        options.advanced.security = SecurityOptions::disabled();

        let resolved =
            options_from_manifest(&v3_manifest(options), ArchiveImportPolicy::Trusted).unwrap();

        assert!(resolved.advanced.nested_virtualization);
        assert_eq!(resolved.advanced.security, SecurityOptions::disabled());
    }

    #[test]
    fn imported_capability_policy_is_validated_before_install() {
        let manifest = ArchiveManifest {
            version: 3,
            box_name: Some("untrusted".into()),
            image: "alpine:latest".into(),
            box_options: Some(BoxOptions {
                advanced: crate::runtime::advanced_options::AdvancedBoxOptions {
                    capabilities: crate::runtime::advanced_options::ContainerCapabilities {
                        drop: vec!["NET-ADMIN".into()],
                        ..Default::default()
                    },
                    ..Default::default()
                },
                ..Default::default()
            }),
            guest_disk_checksum: String::new(),
            container_disk_checksum: String::new(),
            layers: Vec::new(),
            exported_at: "2026-01-01T00:00:00Z".into(),
        };

        let error = options_from_manifest(&manifest, ArchiveImportPolicy::Trusted)
            .expect_err("malformed archived capability policy must be rejected");
        assert!(matches!(error, BoxliteError::InvalidArgument(_)));
        assert!(error.to_string().contains("NET-ADMIN"));
    }

    #[test]
    fn test_validate_no_backing_references_rejects_absolute() {
        let dir = TempDir::new_in("/tmp").unwrap();
        let disk = dir.path().join("evil.qcow2");
        crate::disk::qcow2::write_test_qcow2(&disk, Some("/etc/shadow"));

        let result = validate_no_backing_references(&disk);
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("backing file reference"), "Got: {msg}");
        assert!(msg.contains("/etc/shadow"), "Got: {msg}");
    }

    #[test]
    fn test_validate_no_backing_references_rejects_relative() {
        let dir = TempDir::new_in("/tmp").unwrap();
        let disk = dir.path().join("evil.qcow2");
        crate::disk::qcow2::write_test_qcow2(&disk, Some("../../other/disk.qcow2"));

        let result = validate_no_backing_references(&disk);
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_no_backing_references_accepts_standalone() {
        let dir = TempDir::new_in("/tmp").unwrap();
        let disk = dir.path().join("clean.qcow2");
        crate::disk::qcow2::write_test_qcow2(&disk, None);

        let result = validate_no_backing_references(&disk);
        assert!(result.is_ok());
    }
}

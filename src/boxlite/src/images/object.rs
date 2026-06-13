//! OCI images object with encapsulated operations.
//!
//! This module provides `ImageObject`, a self-contained handle to a pulled
//! OCI image that encapsulates all image-related operations (config loading,
//! layer access, inspection).

use std::path::PathBuf;

use super::blob_source::BlobSource;
use super::manager::ImageManifest;
use boxlite_shared::errors::{BoxliteError, BoxliteResult};

// ============================================================================
// IMAGE OBJECT
// ============================================================================

/// A pulled OCI image with all associated operations.
///
/// This object represents a complete pulled image and provides access to:
/// - Image metadata (reference, layers, config)
/// - Container configuration
/// - Layer file paths
/// - Inspection operations
///
/// Created by `ImageManager::pull()` or `ImageManager::load_from_local()`.
///
/// Thread Safety: `BlobSource` variants handle their own caching strategies.
#[derive(Clone)]
pub struct ImageObject {
    /// Image reference (e.g., "python:alpine")
    reference: String,

    /// Manifest with layer information
    manifest: ImageManifest,

    /// Source of blobs with source-specific caching
    blob_source: BlobSource,
}

impl ImageObject {
    /// Create new ImageObject (internal use only)
    pub(super) fn new(reference: String, manifest: ImageManifest, blob_source: BlobSource) -> Self {
        Self {
            reference,
            manifest,
            blob_source,
        }
    }

    // ========================================================================
    // METADATA OPERATIONS
    // ========================================================================

    /// Get the image reference (e.g., "python:alpine")
    #[allow(dead_code)]
    pub fn reference(&self) -> &str {
        &self.reference
    }

    /// Get list of layer digests
    #[allow(dead_code)]
    pub fn layer_digests(&self) -> Vec<&str> {
        self.manifest
            .layers
            .iter()
            .map(|l| l.digest.as_str())
            .collect()
    }

    /// Get config digest
    #[allow(dead_code)]
    pub fn config_digest(&self) -> &str {
        &self.manifest.config_digest
    }

    /// Get number of layers
    #[allow(dead_code)]
    pub fn layer_count(&self) -> usize {
        self.manifest.layers.len()
    }

    // ========================================================================
    // CONFIG OPERATIONS
    // ========================================================================

    /// Load original OCI image configuration
    ///
    /// Returns the complete OCI ImageConfiguration structure as defined in the
    /// OCI image spec. This includes all fields from the image config.json.
    ///
    /// Use `ContainerConfig::from_oci_config()` if you need extracted container
    /// runtime configuration (entrypoint, env, workdir).
    pub async fn load_config(&self) -> BoxliteResult<oci_spec::image::ImageConfiguration> {
        let config_path = self.blob_source.config_path(&self.manifest.config_digest);
        let config_json = std::fs::read_to_string(&config_path).map_err(|e| {
            BoxliteError::Storage(format!(
                "Failed to read config from {}: {}",
                config_path.display(),
                e
            ))
        })?;

        serde_json::from_str(&config_json)
            .map_err(|e| BoxliteError::Storage(format!("Failed to parse image config: {}", e)))
    }

    // ========================================================================
    // LAYER OPERATIONS
    // ========================================================================

    /// Get path to a specific layer tarball
    ///
    /// Layers are indexed from 0 (base layer) to N-1 (top layer).
    #[allow(dead_code)]
    pub fn layer_tarball(&self, layer_index: usize) -> BoxliteResult<PathBuf> {
        let layer = self.manifest.layers.get(layer_index).ok_or_else(|| {
            BoxliteError::Storage(format!(
                "Layer index {} out of bounds (total layers: {})",
                layer_index,
                self.manifest.layers.len()
            ))
        })?;

        Ok(self.blob_source.layer_tarball_path(&layer.digest))
    }

    /// Get paths to all layer tarballs (ordered bottom to top)
    pub fn layer_tarballs(&self) -> Vec<PathBuf> {
        self.manifest
            .layers
            .iter()
            .map(|layer| self.blob_source.layer_tarball_path(&layer.digest))
            .collect()
    }

    /// Get paths to extracted layer directories (with caching)
    ///
    /// This method extracts each layer tarball to a separate directory and caches
    /// the result. Subsequent calls return the cached extracted directories.
    ///
    /// Uses rayon for parallel extraction of multiple layers.
    ///
    /// This is the VFS-style approach: each layer is extracted once and cached,
    /// then stacked using copy-based mounts.
    ///
    /// # Returns
    /// Vector of paths to extracted layer directories, ordered bottom to top.
    /// Each path is a directory containing the extracted layer contents.
    ///
    /// # Example
    /// ```ignore
    /// let extracted = image.layer_extracted().await?;
    /// // extracted[0] = /images/extracted/sha256:abc.../  (base layer)
    /// // extracted[1] = /images/extracted/sha256:def.../  (layer 1)
    /// // extracted[2] = /images/extracted/sha256:ghi.../  (layer 2)
    /// ```
    pub async fn layer_extracted(&self) -> BoxliteResult<Vec<PathBuf>> {
        let digests: Vec<String> = self
            .manifest
            .layers
            .iter()
            .map(|l| l.digest.clone())
            .collect();

        let extracted = self.blob_source.extract_layers(&digests).await?;

        // Verify DiffIDs if available
        self.verify_diff_ids()?;

        Ok(extracted)
    }

    /// Verify layer DiffIDs against the image config's rootfs.diff_ids.
    ///
    /// DiffIDs are SHA256 hashes of the uncompressed layer tar content.
    /// This ensures the decompressed filesystem content matches what the
    /// image author intended.
    fn verify_diff_ids(&self) -> BoxliteResult<()> {
        use crate::images::archive::LayerVerifier;

        let diff_ids = &self.manifest.diff_ids;
        if diff_ids.is_empty() {
            return Ok(());
        }

        let layers = &self.manifest.layers;
        if diff_ids.len() != layers.len() {
            // The config declares rootfs.diff_ids; OCI requires exactly one per
            // layer. A non-matching count is a malformed or tampered manifest, so
            // fail closed instead of silently skipping verification (which would
            // let an attacker disable DiffID checks by supplying a short list).
            return Err(BoxliteError::Image(format!(
                "DiffID count ({}) does not match layer count ({}); refusing to use image with inconsistent rootfs.diff_ids",
                diff_ids.len(),
                layers.len()
            )));
        }

        for (i, (layer, diff_id)) in layers.iter().zip(diff_ids.iter()).enumerate() {
            let tarball_path = self.blob_source.layer_tarball_path(&layer.digest);
            let verifier = match LayerVerifier::new(diff_id) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!("DiffID parse error for layer {}: {}", i, e);
                    continue;
                }
            };
            match verifier.verify_tarball(&tarball_path) {
                Ok(true) => {
                    tracing::debug!("DiffID verified for layer {}: {}", i, layer.digest);
                }
                Ok(false) => {
                    return Err(BoxliteError::Image(format!(
                        "DiffID verification failed for layer {} ({}): \
                         uncompressed content does not match expected diff_id {}",
                        i, layer.digest, diff_id
                    )));
                }
                Err(e) => {
                    tracing::warn!("DiffID verification error for layer {}: {}", i, e);
                    // Don't fail the pull on verification errors (e.g., unsupported format)
                }
            }
        }

        Ok(())
    }

    /// Compute a stable digest for this image based on its layers.
    ///
    /// This is used as a cache key for base disks - same layers = same base disk.
    /// Uses SHA256 hash of concatenated layer digests.
    pub(crate) fn compute_image_digest(&self) -> String {
        use sha2::{Digest, Sha256};

        let mut hasher = Sha256::new();
        for layer in &self.manifest.layers {
            hasher.update(layer.digest.as_bytes());
        }
        format!("sha256:{:x}", hasher.finalize())
    }

    // ========================================================================
    // INSPECTION
    // ========================================================================

    /// Pretty-print image information
    #[allow(dead_code)]
    pub fn inspect(&self) -> String {
        let mut output = String::new();

        output.push_str(&format!("{}\n", self.reference));
        output.push_str(&format!("Config: {}\n", self.config_digest()));
        output.push_str(&format!("Layers ({}):\n", self.layer_count()));

        for (i, layer) in self.manifest.layers.iter().enumerate() {
            output.push_str(&format!("  {}. {}\n", i + 1, layer.digest));
        }

        output
    }
}

impl std::fmt::Debug for ImageObject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageObject")
            .field("reference", &self.reference)
            .field("layers", &self.manifest.layers.len())
            .field("config_digest", &self.manifest.config_digest)
            .finish()
    }
}

impl std::fmt::Display for ImageObject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} ({} layers)",
            self.reference,
            self.manifest.layers.len()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::images::blob_source::{BlobSource, LocalBundleBlobSource};
    use crate::images::manager::{ImageManifest, LayerInfo};
    use std::path::PathBuf;

    fn layer(digest: &str) -> LayerInfo {
        LayerInfo {
            digest: digest.to_string(),
            media_type: "application/vnd.oci.image.layer.v1.tar+gzip".to_string(),
            size: 1,
        }
    }

    fn object_with(layers: Vec<LayerInfo>, diff_ids: Vec<String>) -> ImageObject {
        let manifest = ImageManifest {
            manifest_digest:
                "sha256:0000000000000000000000000000000000000000000000000000000000000000"
                    .to_string(),
            layers,
            config_digest:
                "sha256:1111111111111111111111111111111111111111111111111111111111111111"
                    .to_string(),
            diff_ids,
        };
        // count-mismatch is rejected before any blob is touched, so a dummy
        // blob source with unused paths is sufficient.
        let blob_source = BlobSource::LocalBundle(LocalBundleBlobSource::new(
            PathBuf::from("/nonexistent/bundle"),
            PathBuf::from("/nonexistent/cache"),
        ));
        ImageObject::new("test:image".to_string(), manifest, blob_source)
    }

    // A config that declares a different number of diff_ids than there are layers
    // is malformed/tampered; verification must fail closed rather than silently
    // skip (which would let an attacker disable DiffID checks). With the fix
    // reverted this returns Ok and the assertion fails.
    #[test]
    fn verify_diff_ids_rejects_count_mismatch() {
        let obj = object_with(
            vec![layer(
                "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            )],
            vec![
                "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                    .to_string(),
                "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
                    .to_string(),
            ],
        );
        assert!(
            obj.verify_diff_ids().is_err(),
            "expected count-mismatch diff_ids to be rejected"
        );
    }

    // Empty diff_ids (e.g. local bundles, or config not yet downloaded) remain a
    // skip, not a hard failure — preserving existing behavior and avoiding
    // breakage of legitimate local-bundle loads.
    #[test]
    fn verify_diff_ids_allows_empty() {
        let obj = object_with(
            vec![layer(
                "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            )],
            vec![],
        );
        assert!(
            obj.verify_diff_ids().is_ok(),
            "empty diff_ids should skip verification"
        );
    }
}

//! Image operations handle
//!
//! Provides `ImageHandle` for performing image-related operations like pulling
//! and listing images. This abstraction separates image management from runtime
//! management, following the same pattern as `LiteBox` for box operations.

use async_trait::async_trait;
use std::sync::Arc;

use crate::BoxliteResult;
use crate::images::ImageObject;
use crate::runtime::types::ImageInfo;

#[cfg(feature = "rest")]
use crate::rest::runtime::RestRuntime;

/// Internal trait for image management.
///
/// Implemented by runtime backends that support local image operations.
#[async_trait]
pub(crate) trait ImageBackend: Send + Sync {
    /// Pull an image from a registry.
    async fn pull_image(&self, image_ref: &str) -> BoxliteResult<ImageObject>;

    /// List all locally cached images.
    async fn list_images(&self) -> BoxliteResult<Vec<ImageInfo>>;
}

/// Metadata returned from a runtime image pull.
///
/// Local runtimes can report the OCI config digest and layer count because they
/// have direct access to the pulled image object. REST runtimes report the
/// server's stable image id (manifest digest) in `config_digest` for backward
/// compatibility with the existing SDK shape, and use `0` when layer count is
/// not available over the wire.
#[derive(Debug, Clone)]
pub struct ImagePullResult {
    pub reference: String,
    pub config_digest: String,
    pub layer_count: usize,
}

impl From<&ImageObject> for ImagePullResult {
    fn from(image: &ImageObject) -> Self {
        Self {
            reference: image.reference().to_string(),
            config_digest: image.config_digest().to_string(),
            layer_count: image.layer_count(),
        }
    }
}

/// Handle for performing image operations.
///
/// Obtained via `BoxliteRuntime::images()`. Provides methods for pulling
/// and listing images.
///
/// # Examples
///
/// ```ignore
/// use boxlite::{Boxlite, Options};
///
/// #[tokio::main]
/// async fn main() -> Result<(), Box<dyn std::error::Error>> {
///     let runtime = Boxlite::new(Options::default())?;
///     let images = runtime.images()?;
///
///     // Pull an image
///     let image = images.pull("alpine:latest").await?;
///     println!("Pulled: {}", image.reference());
///
///     // List all images
///     let all_images = images.list().await?;
///     println!("Total images: {}", all_images.len());
///
///     Ok(())
/// }
/// ```
#[derive(Clone)]
pub struct ImageHandle {
    backend: ImageHandleBackend,
}

#[derive(Clone)]
enum ImageHandleBackend {
    Local(Arc<dyn ImageBackend>),
    #[cfg(feature = "rest")]
    Rest(Arc<RestRuntime>),
}

impl ImageHandle {
    /// Create a new ImageHandle with the given manager.
    ///
    /// This is an internal constructor used by `BoxliteRuntime`.
    pub(crate) fn new(manager: Arc<dyn ImageBackend>) -> Self {
        Self {
            backend: ImageHandleBackend::Local(manager),
        }
    }

    #[cfg(feature = "rest")]
    pub(crate) fn new_rest(rest: Arc<RestRuntime>) -> Self {
        Self {
            backend: ImageHandleBackend::Rest(rest),
        }
    }

    /// Pull an image from a registry.
    ///
    /// Downloads the image layers and stores them in the local image cache.
    /// Returns an ImageObject handle for the pulled image.
    ///
    /// # Example
    ///
    /// ```ignore
    /// # use boxlite::{Boxlite, Options};
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// # let runtime = Boxlite::new(Options::default())?;
    /// let images = runtime.images()?;
    /// let image = images.pull("alpine:latest").await?;
    /// println!("Image digest: {}", image.config_digest());
    /// # Ok(())
    /// # }
    /// ```
    pub async fn pull(&self, image_ref: &str) -> BoxliteResult<ImageObject> {
        match &self.backend {
            ImageHandleBackend::Local(manager) => manager.pull_image(image_ref).await,
            #[cfg(feature = "rest")]
            ImageHandleBackend::Rest(_) => Err(crate::BoxliteError::Unsupported(
                "REST image pulls return metadata only; use pull_info()".to_string(),
            )),
        }
    }

    /// Pull an image and return portable metadata.
    ///
    /// This works for both local and REST runtimes and is the preferred SDK
    /// bridge for image pulls.
    pub async fn pull_info(&self, image_ref: &str) -> BoxliteResult<ImagePullResult> {
        match &self.backend {
            ImageHandleBackend::Local(manager) => {
                let image = manager.pull_image(image_ref).await?;
                Ok(ImagePullResult::from(&image))
            }
            #[cfg(feature = "rest")]
            ImageHandleBackend::Rest(rest) => {
                let info = rest.pull_image_remote(image_ref).await?;
                Ok(ImagePullResult {
                    reference: info.reference,
                    config_digest: info.id,
                    layer_count: 0,
                })
            }
        }
    }

    /// List all locally cached images.
    ///
    /// Returns metadata for all images stored in the local cache.
    ///
    /// # Example
    ///
    /// ```ignore
    /// # use boxlite::{Boxlite, Options};
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// # let runtime = Boxlite::new(Options::default())?;
    /// let images = runtime.images()?;
    /// let all_images = images.list().await?;
    /// for image in all_images {
    ///     println!("{}: {}", image.reference, image.id);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn list(&self) -> BoxliteResult<Vec<ImageInfo>> {
        match &self.backend {
            ImageHandleBackend::Local(manager) => manager.list_images().await,
            #[cfg(feature = "rest")]
            ImageHandleBackend::Rest(rest) => rest.list_images_remote().await,
        }
    }
}

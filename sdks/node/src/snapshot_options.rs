//! Node.js bindings for snapshot, export, and clone options.

use boxlite::{CloneOptions, ExportOptions, SnapshotOptions};
use napi_derive::napi;

/// Options for creating a snapshot (forward-compatible placeholder).
#[napi(object)]
#[derive(Clone, Debug)]
pub struct JsSnapshotOptions {}

impl From<JsSnapshotOptions> for SnapshotOptions {
    fn from(_js: JsSnapshotOptions) -> Self {
        SnapshotOptions {}
    }
}

/// Options for exporting a box.
#[napi(object)]
#[derive(Clone, Debug)]
pub struct JsExportOptions {
    /// Write a directory of content-addressed objects instead of one
    /// `.boxlite` file, so mirroring it to object storage transfers only the
    /// objects the destination lacks.
    pub as_directory: Option<bool>,
    /// Publish into a shared layer store under this archive name (requires
    /// `asDirectory`; the destination is then the store root).
    pub archive_name: Option<String>,
}

impl From<JsExportOptions> for ExportOptions {
    fn from(js: JsExportOptions) -> Self {
        ExportOptions {
            as_directory: js.as_directory.unwrap_or(false),
            archive_name: js.archive_name,
        }
    }
}

/// Options for cloning a box (forward-compatible placeholder).
#[napi(object)]
#[derive(Clone, Debug)]
pub struct JsCloneOptions {}

impl From<JsCloneOptions> for CloneOptions {
    fn from(_js: JsCloneOptions) -> Self {
        CloneOptions {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_options_from_js() {
        let js = JsSnapshotOptions {};
        let _opts: SnapshotOptions = js.into();
    }

    #[test]
    fn export_options_from_js() {
        let js = JsExportOptions {
            as_directory: None,
            archive_name: None,
        };
        let _opts: ExportOptions = js.into();
    }

    #[test]
    fn clone_options_from_js() {
        let js = JsCloneOptions {};
        let _opts: CloneOptions = js.into();
    }
}

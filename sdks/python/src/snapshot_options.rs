//! Python bindings for snapshot, export, and clone options.

use boxlite::{CloneOptions, ExportOptions, SnapshotOptions};
use pyo3::prelude::*;

/// Options for creating a snapshot (forward-compatible placeholder).
#[pyclass(name = "SnapshotOptions")]
#[derive(Clone)]
pub(crate) struct PySnapshotOptions {}

#[pymethods]
impl PySnapshotOptions {
    #[new]
    fn new() -> Self {
        Self {}
    }
}

impl From<PySnapshotOptions> for SnapshotOptions {
    fn from(_py: PySnapshotOptions) -> Self {
        SnapshotOptions {}
    }
}

/// Options for exporting a box.
#[pyclass(name = "ExportOptions")]
#[derive(Clone)]
pub(crate) struct PyExportOptions {
    /// Write a directory of content-addressed objects instead of one
    /// `.boxlite` file, so mirroring it to object storage transfers only the
    /// objects the destination lacks.
    #[pyo3(get, set)]
    pub(crate) as_directory: bool,
}

#[pymethods]
impl PyExportOptions {
    #[new]
    #[pyo3(signature = (as_directory = false))]
    fn new(as_directory: bool) -> Self {
        Self { as_directory }
    }
}

impl From<PyExportOptions> for ExportOptions {
    fn from(py: PyExportOptions) -> Self {
        ExportOptions {
            as_directory: py.as_directory,
        }
    }
}

/// Options for cloning a box (forward-compatible placeholder).
#[pyclass(name = "CloneOptions")]
#[derive(Clone)]
pub(crate) struct PyCloneOptions {}

#[pymethods]
impl PyCloneOptions {
    #[new]
    fn new() -> Self {
        Self {}
    }
}

impl From<PyCloneOptions> for CloneOptions {
    fn from(_py: PyCloneOptions) -> Self {
        CloneOptions {}
    }
}

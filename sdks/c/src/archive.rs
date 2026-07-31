//! Box archive export/import operations for the BoxLite C SDK.

use std::os::raw::{c_char, c_int, c_void};
use std::path::PathBuf;
use std::sync::Arc;

use boxlite::BoxliteError;
use boxlite::runtime::options::{BoxArchive, ExportOptions};

use crate::box_handle::BoxHandle;
use crate::error::{BoxliteErrorCode, FFIError, null_pointer_error, write_error};
use crate::event_queue::{CBoxExportCb, CBoxImportCb, OwnedFfiPtr, RuntimeEvent, push_event};
use crate::runtime::RuntimeHandle;
use crate::util::{alloc_c_string, c_str_to_string};
use crate::{CBoxHandle, CBoxliteError, CBoxliteRuntime};

/// Options for exporting a box archive.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct CArchiveExportOptions {
    /// Non-zero writes a mirrorable directory archive; zero writes one
    /// `.boxlite` file.
    pub as_directory: c_int,
}

/// Exported archive metadata.
///
/// `path` and `sha256` are owned C strings. Release the whole result with
/// `boxlite_free_archive_export_result`.
#[repr(C)]
pub struct CArchiveExportResult {
    pub path: *mut c_char,
    pub sha256: *mut c_char,
    pub size_bytes: u64,
    pub archive_version: u32,
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn boxlite_box_export(
    handle: *mut CBoxHandle,
    dest_path: *const c_char,
    options: CArchiveExportOptions,
    cb: CBoxExportCb,
    user_data: *mut c_void,
    out_error: *mut CBoxliteError,
) -> BoxliteErrorCode {
    box_export(handle, dest_path, options, cb, user_data, out_error)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn boxlite_runtime_import_box(
    runtime: *mut CBoxliteRuntime,
    archive_path: *const c_char,
    name: *const c_char,
    cb: CBoxImportCb,
    user_data: *mut c_void,
    out_error: *mut CBoxliteError,
) -> BoxliteErrorCode {
    runtime_import_box(runtime, archive_path, name, cb, user_data, out_error)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn boxlite_free_archive_export_result(result: *mut CArchiveExportResult) {
    free_archive_export_result(result)
}

unsafe fn box_export(
    handle: *mut BoxHandle,
    dest_path: *const c_char,
    options: CArchiveExportOptions,
    cb: CBoxExportCb,
    user_data: *mut c_void,
    out_error: *mut FFIError,
) -> BoxliteErrorCode {
    unsafe {
        if handle.is_null() {
            write_error(out_error, null_pointer_error("handle"));
            return BoxliteErrorCode::InvalidArgument;
        }
        let dest = match c_str_to_string(dest_path) {
            Ok(s) => PathBuf::from(s),
            Err(e) => {
                write_error(out_error, e);
                return BoxliteErrorCode::InvalidArgument;
            }
        };
        let cb = crate::unwrap_cb_or_return!(cb, out_error);

        let handle_ref = &*handle;
        let lite = handle_ref.handle.clone();
        let queue = handle_ref.queue.clone();
        let user_data_addr = user_data as usize;
        let export_options = ExportOptions {
            as_directory: options.as_directory != 0,
        };

        handle_ref.tokio_rt.spawn(async move {
            let result = lite
                .export(export_options, &dest)
                .await
                .and_then(archive_result_from_box_archive)
                .map(|result| {
                    OwnedFfiPtr::new_with(
                        Box::new(result),
                        free_archive_export_result as unsafe fn(*mut CArchiveExportResult),
                    )
                });
            push_event(
                &queue,
                RuntimeEvent::ExportBox {
                    cb,
                    user_data: user_data_addr,
                    result,
                },
            )
            .await;
        });

        BoxliteErrorCode::Ok
    }
}

unsafe fn runtime_import_box(
    runtime: *mut RuntimeHandle,
    archive_path: *const c_char,
    name: *const c_char,
    cb: CBoxImportCb,
    user_data: *mut c_void,
    out_error: *mut FFIError,
) -> BoxliteErrorCode {
    unsafe {
        if runtime.is_null() {
            write_error(out_error, null_pointer_error("runtime"));
            return BoxliteErrorCode::InvalidArgument;
        }
        let archive_path = match c_str_to_string(archive_path) {
            Ok(s) => PathBuf::from(s),
            Err(e) => {
                write_error(out_error, e);
                return BoxliteErrorCode::InvalidArgument;
            }
        };
        let name = if name.is_null() {
            None
        } else {
            match c_str_to_string(name) {
                Ok(s) => Some(s),
                Err(e) => {
                    write_error(out_error, e);
                    return BoxliteErrorCode::InvalidArgument;
                }
            }
        };
        let cb = crate::unwrap_cb_or_return!(cb, out_error);

        let runtime_ref = &*runtime;
        let runtime_clone = runtime_ref.runtime.clone();
        let tokio_rt = runtime_ref.tokio_rt.clone();
        let queue = runtime_ref.queue.clone();
        let user_data_addr = user_data as usize;
        let task_tokio_rt = tokio_rt.clone();
        let task_queue = queue.clone();

        tokio_rt.spawn(async move {
            let result = runtime_clone
                .import_box(BoxArchive::new(archive_path), name)
                .await
                .map(|handle| {
                    let box_id = handle.id().clone();
                    let boxed = Box::new(BoxHandle {
                        handle: Arc::new(handle),
                        box_id,
                        tokio_rt: task_tokio_rt,
                        queue: task_queue.clone(),
                    });
                    OwnedFfiPtr::new(boxed)
                });
            push_event(
                &queue,
                RuntimeEvent::ImportBox {
                    cb,
                    user_data: user_data_addr,
                    result,
                },
            )
            .await;
        });

        BoxliteErrorCode::Ok
    }
}

fn archive_result_from_box_archive(
    archive: BoxArchive,
) -> Result<CArchiveExportResult, BoxliteError> {
    let path = archive.path().to_string_lossy().into_owned();
    let sha256 = archive
        .sha256()
        .ok_or_else(|| BoxliteError::Internal("export did not return archive sha256".into()))?
        .to_string();
    let size_bytes = archive
        .size_bytes()
        .ok_or_else(|| BoxliteError::Internal("export did not return archive size".into()))?;
    let archive_version = archive
        .archive_version()
        .ok_or_else(|| BoxliteError::Internal("export did not return archive version".into()))?;

    let path = alloc_c_string(&path);
    if path.is_null() {
        return Err(BoxliteError::Internal(
            "archive path contains interior NUL".into(),
        ));
    }
    let sha256 = alloc_c_string(&sha256);
    if sha256.is_null() {
        unsafe { crate::util::free_c_string(path) };
        return Err(BoxliteError::Internal(
            "archive sha256 contains interior NUL".into(),
        ));
    }

    Ok(CArchiveExportResult {
        path,
        sha256,
        size_bytes,
        archive_version,
    })
}

unsafe fn free_archive_export_result(result: *mut CArchiveExportResult) {
    unsafe {
        if result.is_null() {
            return;
        }
        let result = Box::from_raw(result);
        crate::util::free_c_string(result.path);
        crate::util::free_c_string(result.sha256);
    }
}

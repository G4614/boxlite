//! C ABI for `boxlite::runtime::advanced_options::SecurityOptions`.
//!
//! A single opaque handle the caller constructs from one of two profiles,
//! attaches to a box's advanced options via
//! `boxlite_advanced_options_set_security` (which is then applied with
//! `boxlite_options_set_advanced`), and frees with
//! `boxlite_security_options_free`.
//!
//! Two presets are available at construction:
//!   - `boxlite_security_options_new` → `SecurityOptions::enabled()`
//!     (the default; full host-isolation profile).
//!   - `boxlite_security_options_new_disabled` →
//!     `SecurityOptions::disabled()` (master switch off, every
//!     sub-protection off).

use boxlite::runtime::advanced_options::SecurityOptions;

use crate::CSecurityOptions;
use crate::error::{BoxliteErrorCode, FFIError, null_pointer_error, write_error};

/// Opaque handle wrapping a `SecurityOptions`. Allocated via
/// `boxlite_security_options_new` / `_new_disabled`, freed via
/// `boxlite_security_options_free`.
pub struct SecurityOptionsHandle {
    pub options: SecurityOptions,
}

// ─── Lifecycle ─────────────────────────────────────────────────────────────

/// Allocate a `CSecurityOptions` initialized to `SecurityOptions::enabled()`
/// (full host-isolation profile — jailer + seccomp + namespaces + chroot
/// where applicable).
///
/// Sets `*out_opts` to the new handle on `Ok`. The caller owns the handle
/// and must release it via `boxlite_security_options_free` once it has
/// been attached to a `CAdvancedBoxOptions` via
/// `boxlite_advanced_options_set_security` (or if no longer needed).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn boxlite_security_options_new(
    out_opts: *mut *mut CSecurityOptions,
    out_error: *mut FFIError,
) -> BoxliteErrorCode {
    unsafe {
        if out_opts.is_null() {
            write_error(out_error, null_pointer_error("out_opts"));
            return BoxliteErrorCode::InvalidArgument;
        }
        let handle = Box::new(SecurityOptionsHandle {
            options: SecurityOptions::enabled(),
        });
        *out_opts = Box::into_raw(handle);
        BoxliteErrorCode::Ok
    }
}

/// Allocate a `CSecurityOptions` initialized to `SecurityOptions::disabled()`
/// (master switch off, every sub-protection off). Use only for debugging
/// or environments that genuinely can't sandbox.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn boxlite_security_options_new_disabled(
    out_opts: *mut *mut CSecurityOptions,
    out_error: *mut FFIError,
) -> BoxliteErrorCode {
    unsafe {
        if out_opts.is_null() {
            write_error(out_error, null_pointer_error("out_opts"));
            return BoxliteErrorCode::InvalidArgument;
        }
        let handle = Box::new(SecurityOptionsHandle {
            options: SecurityOptions::disabled(),
        });
        *out_opts = Box::into_raw(handle);
        BoxliteErrorCode::Ok
    }
}

/// Release a `CSecurityOptions` previously returned by
/// `boxlite_security_options_new` / `_new_disabled`. Null is a no-op.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn boxlite_security_options_free(opts: *mut CSecurityOptions) {
    if opts.is_null() {
        return;
    }
    unsafe {
        drop(Box::from_raw(opts));
    }
}

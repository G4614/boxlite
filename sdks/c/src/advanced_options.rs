//! C ABI for `boxlite::runtime::advanced_options::AdvancedBoxOptions`.
//!
//! Mirrors the core model: advanced knobs (security, mount isolation, health
//! check) live under `BoxOptions.advanced`, never directly on the box. Build a
//! `CAdvancedBoxOptions` handle via `boxlite_advanced_options_new`, attach a
//! `CSecurityOptions` to it with `boxlite_advanced_options_set_security`, then
//! apply it to a `CBoxliteOptions` via `boxlite_options_set_advanced`.

use boxlite::runtime::advanced_options::AdvancedBoxOptions;

use crate::error::{BoxliteErrorCode, FFIError, null_pointer_error, write_error};
use crate::{CAdvancedBoxOptions, CSecurityOptions};

/// Opaque handle wrapping an `AdvancedBoxOptions`. Allocated via
/// `boxlite_advanced_options_new`, freed via `boxlite_advanced_options_free`.
pub struct AdvancedBoxOptionsHandle {
    pub options: AdvancedBoxOptions,
}

/// Allocate a `CAdvancedBoxOptions` initialized to `AdvancedBoxOptions::default()`
/// (secure-by-default security profile, mount isolation off, no health check).
///
/// Sets `*out_opts` to the new handle on `Ok`. The caller owns the handle and
/// must release it via `boxlite_advanced_options_free` once it has been applied
/// to a `CBoxliteOptions` via `boxlite_options_set_advanced` (or if no longer
/// needed).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn boxlite_advanced_options_new(
    out_opts: *mut *mut CAdvancedBoxOptions,
    out_error: *mut FFIError,
) -> BoxliteErrorCode {
    unsafe {
        if out_opts.is_null() {
            write_error(out_error, null_pointer_error("out_opts"));
            return BoxliteErrorCode::InvalidArgument;
        }
        let handle = Box::new(AdvancedBoxOptionsHandle {
            options: AdvancedBoxOptions::default(),
        });
        *out_opts = Box::into_raw(handle);
        BoxliteErrorCode::Ok
    }
}

/// Release a `CAdvancedBoxOptions` previously returned by
/// `boxlite_advanced_options_new`. Null is a no-op.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn boxlite_advanced_options_free(opts: *mut CAdvancedBoxOptions) {
    if opts.is_null() {
        return;
    }
    unsafe {
        drop(Box::from_raw(opts));
    }
}

/// Attach a fine-grained `CSecurityOptions` to a `CAdvancedBoxOptions`.
/// Clones the security configuration into the advanced options — the caller
/// retains ownership of `security_opts` and frees it via
/// `boxlite_security_options_free`.
///
/// Either pointer being null is a no-op. Build the `CSecurityOptions` handle
/// from a profile (`boxlite_security_options_new` / `_new_disabled`), tweak
/// individual fields, then attach it here.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn boxlite_advanced_options_set_security(
    opts: *mut CAdvancedBoxOptions,
    security_opts: *const CSecurityOptions,
) {
    if opts.is_null() || security_opts.is_null() {
        return;
    }
    unsafe {
        (*opts).options.security = (*security_opts).options.clone();
    }
}

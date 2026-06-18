//! C ABI for `boxlite::runtime::advanced_options::SecurityOptions`.
//!
//! Mirrors the `CBoxliteOptions` shape: a single handle type the caller
//! constructs, mutates via per-field setters, attaches to a box's
//! advanced options via `boxlite_advanced_options_set_security` (which is
//! then applied with `boxlite_options_set_advanced`), and frees with
//! `boxlite_security_options_free`. The setters return `void` and
//! follow the same naming convention as the box-options setters
//! (`boxlite_security_options_set_<field>`).
//!
//! Two presets are available at construction:
//!   - `boxlite_security_options_new` → `SecurityOptions::enabled()`
//!     (the default; full host-isolation profile).
//!   - `boxlite_security_options_new_disabled` →
//!     `SecurityOptions::disabled()` (master switch off, every
//!     sub-protection off).
//!
//! `Option<T>` fields (`uid`, `gid`, `sandbox_profile`, and every
//! `ResourceLimits` cap) expose a setter that takes the value plus a
//! companion `clear_*` setter that resets the field back to `None`.
//! `Vec<String>` fields (`env_allowlist`) expose `add_*` to append and
//! `clear_*` to empty.

use std::ffi::CStr;
use std::os::raw::{c_char, c_int};
use std::path::PathBuf;

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

// ─── Setters: bool fields ──────────────────────────────────────────────────
//
// All bool setters take `c_int` to keep the ABI consistent with the
// existing C SDK (treat any non-zero as true, like `set_auto_remove`).

unsafe fn with_handle<F>(opts: *mut CSecurityOptions, f: F)
where
    F: FnOnce(&mut SecurityOptions),
{
    if opts.is_null() {
        return;
    }
    unsafe { f(&mut (*opts).options) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn boxlite_security_options_set_jailer_enabled(
    opts: *mut CSecurityOptions,
    val: c_int,
) {
    unsafe { with_handle(opts, |o| o.jailer_enabled = val != 0) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn boxlite_security_options_set_seccomp_enabled(
    opts: *mut CSecurityOptions,
    val: c_int,
) {
    unsafe { with_handle(opts, |o| o.seccomp_enabled = val != 0) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn boxlite_security_options_set_new_pid_ns(
    opts: *mut CSecurityOptions,
    val: c_int,
) {
    unsafe { with_handle(opts, |o| o.new_pid_ns = val != 0) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn boxlite_security_options_set_new_net_ns(
    opts: *mut CSecurityOptions,
    val: c_int,
) {
    unsafe { with_handle(opts, |o| o.new_net_ns = val != 0) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn boxlite_security_options_set_chroot_enabled(
    opts: *mut CSecurityOptions,
    val: c_int,
) {
    unsafe { with_handle(opts, |o| o.chroot_enabled = val != 0) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn boxlite_security_options_set_close_fds(
    opts: *mut CSecurityOptions,
    val: c_int,
) {
    unsafe { with_handle(opts, |o| o.close_fds = val != 0) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn boxlite_security_options_set_sanitize_env(
    opts: *mut CSecurityOptions,
    val: c_int,
) {
    unsafe { with_handle(opts, |o| o.sanitize_env = val != 0) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn boxlite_security_options_set_network_enabled(
    opts: *mut CSecurityOptions,
    val: c_int,
) {
    unsafe { with_handle(opts, |o| o.network_enabled = val != 0) }
}

// ─── Setters: Option<u32> fields (uid, gid) ────────────────────────────────

#[unsafe(no_mangle)]
pub unsafe extern "C" fn boxlite_security_options_set_uid(opts: *mut CSecurityOptions, uid: u32) {
    unsafe { with_handle(opts, |o| o.uid = Some(uid)) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn boxlite_security_options_clear_uid(opts: *mut CSecurityOptions) {
    unsafe { with_handle(opts, |o| o.uid = None) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn boxlite_security_options_set_gid(opts: *mut CSecurityOptions, gid: u32) {
    unsafe { with_handle(opts, |o| o.gid = Some(gid)) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn boxlite_security_options_clear_gid(opts: *mut CSecurityOptions) {
    unsafe { with_handle(opts, |o| o.gid = None) }
}

// ─── Setters: PathBuf / Option<PathBuf> fields ─────────────────────────────

/// Set the chroot base directory. `path` must be a valid UTF-8 C string;
/// null or invalid UTF-8 leaves the field untouched.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn boxlite_security_options_set_chroot_base(
    opts: *mut CSecurityOptions,
    path: *const c_char,
) {
    if opts.is_null() || path.is_null() {
        return;
    }
    let Ok(s) = unsafe { CStr::from_ptr(path) }.to_str() else {
        return;
    };
    unsafe { (*opts).options.chroot_base = PathBuf::from(s) }
}

/// Set the macOS sandbox profile override. null = use the built-in profile.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn boxlite_security_options_set_sandbox_profile(
    opts: *mut CSecurityOptions,
    path: *const c_char,
) {
    if opts.is_null() || path.is_null() {
        return;
    }
    let Ok(s) = unsafe { CStr::from_ptr(path) }.to_str() else {
        return;
    };
    unsafe { (*opts).options.sandbox_profile = Some(PathBuf::from(s)) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn boxlite_security_options_clear_sandbox_profile(
    opts: *mut CSecurityOptions,
) {
    unsafe { with_handle(opts, |o| o.sandbox_profile = None) }
}

// ─── Setters: Vec<String> (env_allowlist) ─────────────────────────────────

/// Append a name to the env allowlist. null / invalid UTF-8 = no-op.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn boxlite_security_options_add_env_allowlist(
    opts: *mut CSecurityOptions,
    name: *const c_char,
) {
    if opts.is_null() || name.is_null() {
        return;
    }
    let Ok(s) = unsafe { CStr::from_ptr(name) }.to_str() else {
        return;
    };
    unsafe { (*opts).options.env_allowlist.push(s.to_string()) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn boxlite_security_options_clear_env_allowlist(opts: *mut CSecurityOptions) {
    unsafe { with_handle(opts, |o| o.env_allowlist.clear()) }
}

// ─── Setters: ResourceLimits (5 × Option<u64>) ────────────────────────────

#[unsafe(no_mangle)]
pub unsafe extern "C" fn boxlite_security_options_set_max_open_files(
    opts: *mut CSecurityOptions,
    val: u64,
) {
    unsafe { with_handle(opts, |o| o.resource_limits.max_open_files = Some(val)) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn boxlite_security_options_clear_max_open_files(
    opts: *mut CSecurityOptions,
) {
    unsafe { with_handle(opts, |o| o.resource_limits.max_open_files = None) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn boxlite_security_options_set_max_file_size(
    opts: *mut CSecurityOptions,
    val: u64,
) {
    unsafe { with_handle(opts, |o| o.resource_limits.max_file_size = Some(val)) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn boxlite_security_options_clear_max_file_size(opts: *mut CSecurityOptions) {
    unsafe { with_handle(opts, |o| o.resource_limits.max_file_size = None) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn boxlite_security_options_set_max_processes(
    opts: *mut CSecurityOptions,
    val: u64,
) {
    unsafe { with_handle(opts, |o| o.resource_limits.max_processes = Some(val)) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn boxlite_security_options_clear_max_processes(opts: *mut CSecurityOptions) {
    unsafe { with_handle(opts, |o| o.resource_limits.max_processes = None) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn boxlite_security_options_set_max_memory(
    opts: *mut CSecurityOptions,
    val: u64,
) {
    unsafe { with_handle(opts, |o| o.resource_limits.max_memory = Some(val)) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn boxlite_security_options_clear_max_memory(opts: *mut CSecurityOptions) {
    unsafe { with_handle(opts, |o| o.resource_limits.max_memory = None) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn boxlite_security_options_set_max_cpu_time(
    opts: *mut CSecurityOptions,
    val: u64,
) {
    unsafe { with_handle(opts, |o| o.resource_limits.max_cpu_time = Some(val)) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn boxlite_security_options_clear_max_cpu_time(opts: *mut CSecurityOptions) {
    unsafe { with_handle(opts, |o| o.resource_limits.max_cpu_time = None) }
}

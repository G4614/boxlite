// Fine-grained SecurityOptions wrapper.
//
// `WithSecurityOptions(spec)` is the Go counterpart of the C
// `boxlite_options_set_security` setter — callers build a
// `*SecurityOptions` via `NewSecurityOptions` (= the fully-isolated
// default) or `NewSecurityOptionsDisabled` (the explicit opt-out),
// optionally tweak individual fields, and pass it to
// `runtime.Create(..., boxlite.WithSecurityOptions(spec))`.
//
// The lighter `WithSecurity(enabled bool)` shortcut was removed in
// favor of this richer API. To get the equivalent of the old
// `WithSecurity(true)` / `WithSecurity(false)` just construct one of
// the presets and pass it through:
//
//	enabled, _  := boxlite.NewSecurityOptions()
//	defer enabled.Close()
//	box, _ := runtime.Create(ctx, "alpine:latest",
//	    boxlite.WithSecurityOptions(enabled))

package boxlite

/*
#include <stdlib.h>
#include "boxlite.h"
*/
import "C"

import (
	"runtime"
	"unsafe"
)

// SecurityOptions is the Go-side handle for a `CSecurityOptions`.
// Construct via `NewSecurityOptions` / `NewSecurityOptionsDisabled`;
// release via `Close` once the spec has been attached to a box (or
// you no longer need it).
type SecurityOptions struct {
	handle *C.CSecurityOptions
}

// NewSecurityOptions returns the fully-enabled security profile
// (`SecurityOptions::enabled()` in Rust) — jailer + seccomp + namespaces
// + chroot + unprivileged uid/gid + closed fds + sanitized env. This is
// the runtime default; constructing it explicitly is useful only when
// you want to further override individual fields before attaching.
func NewSecurityOptions() (*SecurityOptions, error) {
	var raw *C.CSecurityOptions
	var cerr C.CBoxliteError
	if code := C.boxlite_security_options_new(&raw, &cerr); code != C.Ok {
		return nil, errorFromCError(&cerr)
	}
	s := &SecurityOptions{handle: raw}
	runtime.SetFinalizer(s, func(s *SecurityOptions) { s.Close() })
	return s, nil
}

// NewSecurityOptionsDisabled returns the explicit opt-out profile
// (`SecurityOptions::disabled()` in Rust) — master switch off, every
// sub-protection off. Use only for debugging or environments that
// genuinely can't sandbox.
func NewSecurityOptionsDisabled() (*SecurityOptions, error) {
	var raw *C.CSecurityOptions
	var cerr C.CBoxliteError
	if code := C.boxlite_security_options_new_disabled(&raw, &cerr); code != C.Ok {
		return nil, errorFromCError(&cerr)
	}
	s := &SecurityOptions{handle: raw}
	runtime.SetFinalizer(s, func(s *SecurityOptions) { s.Close() })
	return s, nil
}

// Close releases the underlying CSecurityOptions. Idempotent.
func (s *SecurityOptions) Close() {
	if s == nil || s.handle == nil {
		return
	}
	C.boxlite_security_options_free(s.handle)
	s.handle = nil
	runtime.SetFinalizer(s, nil)
}

// ─── bool setters ──────────────────────────────────────────────────────────

func (s *SecurityOptions) SetJailerEnabled(v bool) {
	if s == nil || s.handle == nil {
		return
	}
	C.boxlite_security_options_set_jailer_enabled(s.handle, boolToCInt(v))
}

func (s *SecurityOptions) SetSeccompEnabled(v bool) {
	if s == nil || s.handle == nil {
		return
	}
	C.boxlite_security_options_set_seccomp_enabled(s.handle, boolToCInt(v))
}

func (s *SecurityOptions) SetNewPIDNamespace(v bool) {
	if s == nil || s.handle == nil {
		return
	}
	C.boxlite_security_options_set_new_pid_ns(s.handle, boolToCInt(v))
}

func (s *SecurityOptions) SetNewNetNamespace(v bool) {
	if s == nil || s.handle == nil {
		return
	}
	C.boxlite_security_options_set_new_net_ns(s.handle, boolToCInt(v))
}

func (s *SecurityOptions) SetChrootEnabled(v bool) {
	if s == nil || s.handle == nil {
		return
	}
	C.boxlite_security_options_set_chroot_enabled(s.handle, boolToCInt(v))
}

func (s *SecurityOptions) SetCloseFDs(v bool) {
	if s == nil || s.handle == nil {
		return
	}
	C.boxlite_security_options_set_close_fds(s.handle, boolToCInt(v))
}

func (s *SecurityOptions) SetSanitizeEnv(v bool) {
	if s == nil || s.handle == nil {
		return
	}
	C.boxlite_security_options_set_sanitize_env(s.handle, boolToCInt(v))
}

// SetNetworkEnabled toggles the sandbox profile's network policy
// (Linux landlock + macOS seatbelt). Not to be confused with `WithNetwork` /
// `NetworkSpec`, which is the guest VM's network plumbing.
func (s *SecurityOptions) SetNetworkEnabled(v bool) {
	if s == nil || s.handle == nil {
		return
	}
	C.boxlite_security_options_set_network_enabled(s.handle, boolToCInt(v))
}

// ─── Option<u32> setters (uid, gid) ────────────────────────────────────────

func (s *SecurityOptions) SetUID(uid uint32) {
	if s == nil || s.handle == nil {
		return
	}
	C.boxlite_security_options_set_uid(s.handle, C.uint32_t(uid))
}

func (s *SecurityOptions) ClearUID() {
	if s == nil || s.handle == nil {
		return
	}
	C.boxlite_security_options_clear_uid(s.handle)
}

func (s *SecurityOptions) SetGID(gid uint32) {
	if s == nil || s.handle == nil {
		return
	}
	C.boxlite_security_options_set_gid(s.handle, C.uint32_t(gid))
}

func (s *SecurityOptions) ClearGID() {
	if s == nil || s.handle == nil {
		return
	}
	C.boxlite_security_options_clear_gid(s.handle)
}

// ─── PathBuf / Option<PathBuf> setters ─────────────────────────────────────

// SetChrootBase sets the base directory under which per-box chroot
// jails are constructed (Linux only).
func (s *SecurityOptions) SetChrootBase(path string) {
	if s == nil || s.handle == nil {
		return
	}
	c := toCString(path)
	defer C.free(unsafe.Pointer(c))
	C.boxlite_security_options_set_chroot_base(s.handle, c)
}

// SetSandboxProfile sets the macOS sandbox profile override.
// Empty string is treated as no-op; use `ClearSandboxProfile` to reset
// to the built-in profile.
func (s *SecurityOptions) SetSandboxProfile(path string) {
	if s == nil || s.handle == nil || path == "" {
		return
	}
	c := toCString(path)
	defer C.free(unsafe.Pointer(c))
	C.boxlite_security_options_set_sandbox_profile(s.handle, c)
}

func (s *SecurityOptions) ClearSandboxProfile() {
	if s == nil || s.handle == nil {
		return
	}
	C.boxlite_security_options_clear_sandbox_profile(s.handle)
}

// ─── env_allowlist (Vec<String>) ───────────────────────────────────────────

// AddEnvAllowlist appends `name` to the env-allowlist. Repeated calls
// accumulate. Empty / whitespace-only names are dropped silently.
func (s *SecurityOptions) AddEnvAllowlist(name string) {
	if s == nil || s.handle == nil || name == "" {
		return
	}
	c := toCString(name)
	defer C.free(unsafe.Pointer(c))
	C.boxlite_security_options_add_env_allowlist(s.handle, c)
}

func (s *SecurityOptions) ClearEnvAllowlist() {
	if s == nil || s.handle == nil {
		return
	}
	C.boxlite_security_options_clear_env_allowlist(s.handle)
}

// ─── ResourceLimits (5 × Option<u64>) ──────────────────────────────────────

func (s *SecurityOptions) SetMaxOpenFiles(v uint64) {
	if s == nil || s.handle == nil {
		return
	}
	C.boxlite_security_options_set_max_open_files(s.handle, C.uint64_t(v))
}

func (s *SecurityOptions) ClearMaxOpenFiles() {
	if s == nil || s.handle == nil {
		return
	}
	C.boxlite_security_options_clear_max_open_files(s.handle)
}

func (s *SecurityOptions) SetMaxFileSize(v uint64) {
	if s == nil || s.handle == nil {
		return
	}
	C.boxlite_security_options_set_max_file_size(s.handle, C.uint64_t(v))
}

func (s *SecurityOptions) ClearMaxFileSize() {
	if s == nil || s.handle == nil {
		return
	}
	C.boxlite_security_options_clear_max_file_size(s.handle)
}

func (s *SecurityOptions) SetMaxProcesses(v uint64) {
	if s == nil || s.handle == nil {
		return
	}
	C.boxlite_security_options_set_max_processes(s.handle, C.uint64_t(v))
}

func (s *SecurityOptions) ClearMaxProcesses() {
	if s == nil || s.handle == nil {
		return
	}
	C.boxlite_security_options_clear_max_processes(s.handle)
}

func (s *SecurityOptions) SetMaxMemory(v uint64) {
	if s == nil || s.handle == nil {
		return
	}
	C.boxlite_security_options_set_max_memory(s.handle, C.uint64_t(v))
}

func (s *SecurityOptions) ClearMaxMemory() {
	if s == nil || s.handle == nil {
		return
	}
	C.boxlite_security_options_clear_max_memory(s.handle)
}

func (s *SecurityOptions) SetMaxCPUTime(v uint64) {
	if s == nil || s.handle == nil {
		return
	}
	C.boxlite_security_options_set_max_cpu_time(s.handle, C.uint64_t(v))
}

func (s *SecurityOptions) ClearMaxCPUTime() {
	if s == nil || s.handle == nil {
		return
	}
	C.boxlite_security_options_clear_max_cpu_time(s.handle)
}

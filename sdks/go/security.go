// SecurityOptions wrapper.
//
// A SecurityOptions profile is reached through the advanced layer: pick a
// profile with `NewSecurityOptions` (= the fully-isolated default) or
// `NewSecurityOptionsDisabled` (the explicit opt-out), attach it to an
// `AdvancedBoxOptions` via `SetSecurity`, then pass that to
// `runtime.Create(..., boxlite.WithAdvancedOptions(adv))`. This mirrors the
// core `BoxOptions.advanced.security` model.
//
//	adv, _ := boxlite.NewAdvancedBoxOptions()
//	defer adv.Close()
//	sec, _ := boxlite.NewSecurityOptionsDisabled()
//	defer sec.Close()
//	adv.SetSecurity(sec)
//	box, _ := runtime.Create(ctx, "alpine:latest",
//	    boxlite.WithAdvancedOptions(adv))

package boxlite

/*
#include <stdlib.h>
#include "boxlite.h"
*/
import "C"

import "runtime"

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

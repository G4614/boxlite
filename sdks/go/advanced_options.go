// AdvancedBoxOptions groups the box-level advanced knobs (currently the
// security profile) under one handle, mirroring core `BoxOptions.advanced`.
//
// Build it via `NewAdvancedBoxOptions`, attach a security profile with
// `SetSecurity`, and pass it to `runtime.Create(..., WithAdvancedOptions(adv))`.
// Security is reached through this layer (never attached to the box directly),
// matching the core `BoxOptions.advanced.security` model.
//
//	adv, _ := boxlite.NewAdvancedBoxOptions()
//	defer adv.Close()
//	sec, _ := boxlite.NewSecurityOptionsDisabled()
//	defer sec.Close()
//	adv.SetSecurity(sec)
//	box, _ := runtime.Create(ctx, "alpine:latest", boxlite.WithAdvancedOptions(adv))

package boxlite

/*
#include "boxlite.h"
*/
import "C"

import "runtime"

// AdvancedBoxOptions is the Go-side handle for a `CAdvancedBoxOptions`.
// Construct via `NewAdvancedBoxOptions`; release via `Close` once it has
// been attached to a box (or you no longer need it).
type AdvancedBoxOptions struct {
	handle *C.CAdvancedBoxOptions
}

// NewAdvancedBoxOptions allocates an advanced-options handle initialized to
// the defaults (secure-by-default security profile, mount isolation off, no
// health check).
func NewAdvancedBoxOptions() (*AdvancedBoxOptions, error) {
	var raw *C.CAdvancedBoxOptions
	var cerr C.CBoxliteError
	if code := C.boxlite_advanced_options_new(&raw, &cerr); code != C.Ok {
		return nil, errorFromCError(&cerr)
	}
	a := &AdvancedBoxOptions{handle: raw}
	runtime.SetFinalizer(a, func(a *AdvancedBoxOptions) { a.Close() })
	return a, nil
}

// SetSecurity attaches a SecurityOptions profile to the advanced options.
// The caller retains ownership of `spec` (and must `Close` it); the advanced
// options take their own copy. Nil receiver or spec is a no-op.
func (a *AdvancedBoxOptions) SetSecurity(spec *SecurityOptions) {
	if a == nil || a.handle == nil || spec == nil || spec.handle == nil {
		return
	}
	C.boxlite_advanced_options_set_security(a.handle, spec.handle)
}

// Close releases the underlying CAdvancedBoxOptions. Idempotent.
func (a *AdvancedBoxOptions) Close() {
	if a == nil || a.handle == nil {
		return
	}
	C.boxlite_advanced_options_free(a.handle)
	a.handle = nil
	runtime.SetFinalizer(a, nil)
}

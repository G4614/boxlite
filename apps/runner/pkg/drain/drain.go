// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 BoxLite AI

package drain

import (
	"sync"
	"sync/atomic"
)

var (
	draining     atomic.Bool
	activeAttach atomic.Int64
	attachGate   sync.Mutex
)

func Enable() {
	attachGate.Lock()
	defer attachGate.Unlock()
	draining.Store(true)
}

func Disable() {
	attachGate.Lock()
	defer attachGate.Unlock()
	draining.Store(false)
}

func IsDraining() bool {
	return draining.Load()
}

func TryBeginAttach() bool {
	attachGate.Lock()
	defer attachGate.Unlock()
	if draining.Load() {
		return false
	}
	activeAttach.Add(1)
	return true
}

func EndAttach() {
	activeAttach.Add(-1)
}

func ActiveAttachCount() int64 {
	return activeAttach.Load()
}

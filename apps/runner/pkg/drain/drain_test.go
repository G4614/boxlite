// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 BoxLite AI

package drain

import "testing"

func TestTryBeginAttachHonorsDrainGate(t *testing.T) {
	Disable()
	if !TryBeginAttach() {
		t.Fatal("attach should start while the runner is accepting work")
	}
	if ActiveAttachCount() != 1 {
		t.Fatalf("active attach count = %d, want 1", ActiveAttachCount())
	}
	EndAttach()

	Enable()
	t.Cleanup(Disable)
	if TryBeginAttach() {
		t.Fatal("attach must not start after draining is enabled")
	}
	if ActiveAttachCount() != 0 {
		t.Fatalf("rejected attach changed count to %d", ActiveAttachCount())
	}
}

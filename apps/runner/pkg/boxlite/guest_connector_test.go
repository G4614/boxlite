package boxlite

import (
	"fmt"
	"os"
	"path/filepath"
	"testing"
)

func TestGvproxyGuestConnectorSocketPathForRuntimeBoxIDUsesRustBindingPath(t *testing.T) {
	got, err := gvproxyGuestConnectorSocketPathForRuntimeBoxID("box-123")
	if err != nil {
		t.Fatalf("gvproxyGuestConnectorSocketPathForRuntimeBoxID: %v", err)
	}

	want := filepath.Join("/tmp", fmt.Sprintf("bl-%d", os.Getuid()), "box-123", "net.sock") + ".guest"
	if got != want {
		t.Fatalf("GvproxyGuestConnectorSocketPath = %q, want %q", got, want)
	}
}

func TestGvproxyGuestConnectorSocketPathForRuntimeBoxIDRejectsUnsafeBoxIDs(t *testing.T) {
	for _, boxId := range []string{"", ".", "..", "../box", `bad\box`} {
		t.Run(boxId, func(t *testing.T) {
			if got, err := gvproxyGuestConnectorSocketPathForRuntimeBoxID(boxId); err == nil {
				t.Fatalf("gvproxyGuestConnectorSocketPathForRuntimeBoxID(%q) = %q, want error", boxId, got)
			}
		})
	}
}

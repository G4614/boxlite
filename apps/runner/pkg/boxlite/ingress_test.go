package boxlite

import (
	"fmt"
	"os"
	"path/filepath"
	"testing"
)

func TestGvproxyIngressSocketPathForRuntimeBoxIDUsesRustBindingPath(t *testing.T) {
	got, err := gvproxyIngressSocketPathForRuntimeBoxID("box-123")
	if err != nil {
		t.Fatalf("gvproxyIngressSocketPathForRuntimeBoxID: %v", err)
	}

	want := filepath.Join("/tmp", fmt.Sprintf("bl-%d", os.Getuid()), "box-123", "net.sock") + ".ingress"
	if got != want {
		t.Fatalf("GvproxyIngressSocketPath = %q, want %q", got, want)
	}
}

func TestGvproxyIngressSocketPathForRuntimeBoxIDRejectsUnsafeBoxIDs(t *testing.T) {
	for _, boxId := range []string{"", ".", "..", "../box", `bad\box`} {
		t.Run(boxId, func(t *testing.T) {
			if got, err := gvproxyIngressSocketPathForRuntimeBoxID(boxId); err == nil {
				t.Fatalf("gvproxyIngressSocketPathForRuntimeBoxID(%q) = %q, want error", boxId, got)
			}
		})
	}
}

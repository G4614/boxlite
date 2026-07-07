package boxlite

import (
	"context"
	"fmt"
	"os"
	"path/filepath"
	"strings"
)

const (
	socketSymlinkBase    = "/tmp"
	netSocketName        = "net.sock"
	guestConnectorSuffix = ".guest"
)

// GvproxyGuestConnectorSocketPath returns the short Unix socket path used by
// gvproxy's internal guest-port connector. The public/control-plane box id may
// be the box name, so resolve the box first and then use the core runtime id in
// the Rust socket path.
func (c *Client) GvproxyGuestConnectorSocketPath(ctx context.Context, boxId string) (string, error) {
	if err := validateBoxIdForSocketPath(boxId); err != nil {
		return "", err
	}

	bx, err := c.getOrFetchBox(ctx, boxId)
	if err != nil {
		return "", err
	}

	return gvproxyGuestConnectorSocketPathForRuntimeBoxID(bx.ID())
}

func gvproxyGuestConnectorSocketPathForRuntimeBoxID(runtimeBoxID string) (string, error) {
	if err := validateBoxIdForSocketPath(runtimeBoxID); err != nil {
		return "", err
	}

	return filepath.Join(socketSymlinkBase, fmt.Sprintf("bl-%d", os.Getuid()), runtimeBoxID, netSocketName) + guestConnectorSuffix, nil
}

func validateBoxIdForSocketPath(boxId string) error {
	if boxId == "" {
		return fmt.Errorf("box id is required")
	}
	if boxId == "." || boxId == ".." || strings.ContainsAny(boxId, `/\`) {
		return fmt.Errorf("invalid box id %q", boxId)
	}
	return nil
}

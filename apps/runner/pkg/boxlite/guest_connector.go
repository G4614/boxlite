package boxlite

import (
	"context"
	"fmt"
	"os"
	"path/filepath"
	"strings"
)

const (
	socketSymlinkBase     = "/tmp"
	netSocketName         = "net.sock"
	guestConnectorSuffix  = ".guest"
	defaultGvproxyGuestIP = "192.168.127.2"
)

// GvproxyGuestConnectorEndpoint returns the Unix socket endpoint used by
// gvproxy's internal ServicesMux tunnel. The public/control-plane box id may be
// the box name, so resolve the box first and then use the core runtime id in the
// Rust socket path.
func (c *Client) GvproxyGuestConnectorEndpoint(ctx context.Context, boxId string) (string, string, error) {
	if err := validateBoxIdForSocketPath(boxId); err != nil {
		return "", "", err
	}

	bx, err := c.getOrFetchBox(ctx, boxId)
	if err != nil {
		return "", "", err
	}

	socketPath, err := gvproxyGuestConnectorSocketPathForRuntimeBoxID(bx.ID())
	if err != nil {
		return "", "", err
	}

	// This matches src/boxlite/src/net/constants.rs::GUEST_IP. Core currently
	// gives every guest the same DHCP static lease.
	return socketPath, defaultGvproxyGuestIP, nil
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

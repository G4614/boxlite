package boxlite

import (
	"context"
	"fmt"
	"log/slog"
	"net"
	"net/http"

	sdk "github.com/boxlite-ai/boxlite/sdks/go"
)

func (c *Client) NewGuestPortTransport(boxId string, port uint16, logger *slog.Logger) *http.Transport {
	if logger == nil {
		logger = slog.Default()
	}

	return &http.Transport{
		ForceAttemptHTTP2: false,
		DialContext: func(ctx context.Context, network string, addr string) (net.Conn, error) {
			if network != "tcp" && network != "tcp4" && network != "tcp6" {
				return nil, fmt.Errorf("unsupported network %q", network)
			}

			conn, err := c.DialGuestPort(ctx, boxId, port)
			if err != nil {
				logger.WarnContext(ctx, "guest port tunnel failed", "box", boxId, "port", port, "error", err)
				return nil, err
			}
			return conn, nil
		},
	}
}

func (c *Client) DialGuestPort(ctx context.Context, boxId string, port uint16) (net.Conn, error) {
	return c.DialGuestTCP(ctx, boxId, net.JoinHostPort(sdk.GuestIP, fmt.Sprintf("%d", port)))
}

func (c *Client) DialGuestTCP(ctx context.Context, boxId string, target string) (net.Conn, error) {
	bx, err := c.getOrFetchBox(ctx, boxId)
	if err != nil {
		return nil, err
	}

	network, err := bx.Network()
	if err != nil {
		return nil, fmt.Errorf("open box network handle for %s: %w", boxId, err)
	}
	defer network.Close()

	conn, err := network.Tunnel(ctx, target)
	if err != nil {
		return nil, fmt.Errorf("open guest TCP tunnel to %s/%s: %w", boxId, target, err)
	}
	return conn, nil
}

// Copyright 2026 BoxLite AI
// SPDX-License-Identifier: AGPL-3.0

package portgateway

import (
	"bufio"
	"context"
	"fmt"
	"io"
	"log/slog"
	"net"
	"net/http"
	"sync"
	"time"

	"github.com/containers/gvisor-tap-vsock/pkg/transport"
)

const (
	gvproxyTunnelTimeout = 10 * time.Second
)

type ConnectorResolver interface {
	GvproxyGuestConnectorEndpoint(ctx context.Context, boxId string) (socketPath string, guestIP string, err error)
}

type Gateway struct {
	resolver ConnectorResolver
	logger   *slog.Logger
}

func New(resolver ConnectorResolver, logger *slog.Logger) *Gateway {
	if logger == nil {
		logger = slog.Default()
	}
	return &Gateway{
		resolver: resolver,
		logger:   logger,
	}
}

func (g *Gateway) ServeConnect(w http.ResponseWriter, req *http.Request, boxId string, port uint16) {
	connectorSocketPath, guestIP, err := g.resolver.GvproxyGuestConnectorEndpoint(req.Context(), boxId)
	if err != nil {
		http.Error(w, err.Error(), http.StatusBadRequest)
		return
	}

	guestConn, err := dialGvproxyTunnel(req.Context(), connectorSocketPath, guestIP, port)
	if err != nil {
		g.logger.WarnContext(req.Context(), "guest port tunnel failed", "box", boxId, "port", port, "error", err)
		http.Error(w, "guest port tunnel failed: "+err.Error(), http.StatusBadGateway)
		return
	}

	hijacker, ok := w.(http.Hijacker)
	if !ok {
		_ = guestConn.Close()
		http.Error(w, "response writer does not support hijacking", http.StatusInternalServerError)
		return
	}

	clientConn, rw, err := hijacker.Hijack()
	if err != nil {
		_ = guestConn.Close()
		g.logger.WarnContext(req.Context(), "guest port tunnel hijack failed", "box", boxId, "port", port, "error", err)
		return
	}
	defer clientConn.Close()
	defer guestConn.Close()

	if _, err := rw.WriteString("HTTP/1.1 200 Connection Established\r\n\r\n"); err != nil {
		g.logger.WarnContext(req.Context(), "guest port tunnel response failed", "box", boxId, "port", port, "error", err)
		return
	}
	if err := rw.Flush(); err != nil {
		g.logger.WarnContext(req.Context(), "guest port tunnel flush failed", "box", boxId, "port", port, "error", err)
		return
	}

	result := Pump(req.Context(), &bufferedTunnelConn{Conn: clientConn, reader: rw.Reader}, guestConn)
	g.logger.DebugContext(
		req.Context(),
		"guest port tunnel closed",
		"box", boxId,
		"port", port,
		"client_to_guest_bytes", result.ClientToGuestBytes,
		"guest_to_client_bytes", result.GuestToClientBytes,
	)
}

func dialGvproxyTunnel(ctx context.Context, connectorSocketPath string, guestIP string, port uint16) (net.Conn, error) {
	if guestIP == "" {
		return nil, fmt.Errorf("guest IP is required")
	}

	dialer := net.Dialer{}
	conn, err := dialer.DialContext(ctx, "unix", connectorSocketPath)
	if err != nil {
		return nil, fmt.Errorf("dial gvproxy tunnel socket %s: %w", connectorSocketPath, err)
	}

	deadline := time.Now().Add(gvproxyTunnelTimeout)
	if ctxDeadline, ok := ctx.Deadline(); ok && ctxDeadline.Before(deadline) {
		deadline = ctxDeadline
	}
	if err := conn.SetDeadline(deadline); err != nil {
		_ = conn.Close()
		return nil, err
	}

	if err := transport.Tunnel(conn, guestIP, int(port)); err != nil {
		_ = conn.Close()
		return nil, fmt.Errorf("open gvproxy tunnel to %s:%d: %w", guestIP, port, err)
	}

	if err := conn.SetDeadline(time.Time{}); err != nil {
		_ = conn.Close()
		return nil, err
	}

	return conn, nil
}

type PumpResult struct {
	ClientToGuestBytes int64
	GuestToClientBytes int64
}

// Pump treats both sides as opaque TCP streams. It must not parse HTTP,
// WebSocket, paths, headers, or any other application-layer data.
func Pump(ctx context.Context, client net.Conn, guest net.Conn) PumpResult {
	var result PumpResult
	var wg sync.WaitGroup
	wg.Add(2)

	done := make(chan struct{})
	if ctx != nil {
		go func() {
			select {
			case <-ctx.Done():
				_ = client.Close()
				_ = guest.Close()
			case <-done:
			}
		}()
	}

	go func() {
		defer wg.Done()
		result.ClientToGuestBytes, _ = io.Copy(guest, client)
		closeWriteOrClose(guest)
	}()

	go func() {
		defer wg.Done()
		result.GuestToClientBytes, _ = io.Copy(client, guest)
		closeWriteOrClose(client)
	}()

	wg.Wait()
	close(done)
	return result
}

func closeWriteOrClose(conn net.Conn) {
	if closeWriter, ok := conn.(interface{ CloseWrite() error }); ok {
		_ = closeWriter.CloseWrite()
		return
	}
	_ = conn.Close()
}

type bufferedTunnelConn struct {
	net.Conn
	reader *bufio.Reader
}

func (c *bufferedTunnelConn) Read(p []byte) (int, error) {
	return c.reader.Read(p)
}

// Copyright 2026 BoxLite AI
// SPDX-License-Identifier: AGPL-3.0

package portgateway

import (
	"bufio"
	"context"
	"encoding/binary"
	"fmt"
	"io"
	"log/slog"
	"net"
	"net/http"
	"strings"
	"sync"
	"time"
)

const (
	guestConnectorTimeout = 10 * time.Second
	guestConnectorMagic   = "BLGC1"
)

type ConnectorResolver interface {
	GvproxyGuestConnectorSocketPath(ctx context.Context, boxId string) (string, error)
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
	connectorSocketPath, err := g.resolver.GvproxyGuestConnectorSocketPath(req.Context(), boxId)
	if err != nil {
		http.Error(w, err.Error(), http.StatusBadRequest)
		return
	}

	guestConn, err := dialGvproxyGuestConnector(req.Context(), connectorSocketPath, port)
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

func dialGvproxyGuestConnector(ctx context.Context, connectorSocketPath string, port uint16) (net.Conn, error) {
	dialer := net.Dialer{}
	conn, err := dialer.DialContext(ctx, "unix", connectorSocketPath)
	if err != nil {
		return nil, fmt.Errorf("dial gvproxy guest connector %s: %w", connectorSocketPath, err)
	}

	deadline := time.Now().Add(guestConnectorTimeout)
	if ctxDeadline, ok := ctx.Deadline(); ok && ctxDeadline.Before(deadline) {
		deadline = ctxDeadline
	}
	if err := conn.SetDeadline(deadline); err != nil {
		_ = conn.Close()
		return nil, err
	}

	request := make([]byte, len(guestConnectorMagic)+2)
	copy(request, guestConnectorMagic)
	binary.BigEndian.PutUint16(request[len(guestConnectorMagic):], port)
	if _, err := conn.Write(request); err != nil {
		_ = conn.Close()
		return nil, fmt.Errorf("write gvproxy guest connector request: %w", err)
	}

	reader := bufio.NewReaderSize(conn, 128)
	line, err := reader.ReadString('\n')
	if err != nil {
		_ = conn.Close()
		return nil, fmt.Errorf("read gvproxy guest connector response: %w", err)
	}
	if strings.TrimSpace(line) != "OK" {
		_ = conn.Close()
		return nil, fmt.Errorf("gvproxy guest connector rejected port %d: %s", port, strings.TrimSpace(line))
	}

	if err := conn.SetDeadline(time.Time{}); err != nil {
		_ = conn.Close()
		return nil, err
	}

	return &guestConnectorHandshakeConn{Conn: conn, reader: reader}, nil
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

type guestConnectorHandshakeConn struct {
	net.Conn
	reader *bufio.Reader
}

func (c *guestConnectorHandshakeConn) Read(p []byte) (int, error) {
	return c.reader.Read(p)
}

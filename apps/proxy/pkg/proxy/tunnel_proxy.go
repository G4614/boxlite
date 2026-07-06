// Copyright 2026 BoxLite AI
// SPDX-License-Identifier: AGPL-3.0

package proxy

import (
	"bufio"
	"context"
	"crypto/tls"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/url"
	"strings"
	"time"
)

const runnerTunnelHandshakeTimeout = 10 * time.Second

func newRunnerTunnelTransport(tunnelURL *url.URL, headers map[string]string) http.RoundTripper {
	return &runnerTunnelTransport{
		tunnelURL: tunnelURL,
		headers:   headers,
	}
}

type runnerTunnelTransport struct {
	tunnelURL *url.URL
	headers   map[string]string
}

func (t *runnerTunnelTransport) RoundTrip(req *http.Request) (*http.Response, error) {
	transport := &http.Transport{
		DisableKeepAlives: true,
		DialContext: func(ctx context.Context, network string, addr string) (net.Conn, error) {
			return t.dialTunnel(ctx)
		},
	}
	defer transport.CloseIdleConnections()

	return transport.RoundTrip(req)
}

func (t *runnerTunnelTransport) dialTunnel(ctx context.Context) (net.Conn, error) {
	conn, err := dialRunnerControl(ctx, t.tunnelURL)
	if err != nil {
		return nil, err
	}

	deadline := time.Now().Add(runnerTunnelHandshakeTimeout)
	if ctxDeadline, ok := ctx.Deadline(); ok && ctxDeadline.Before(deadline) {
		deadline = ctxDeadline
	}
	if err := conn.SetDeadline(deadline); err != nil {
		_ = conn.Close()
		return nil, err
	}

	if err := writeTunnelConnect(conn, t.tunnelURL, t.headers); err != nil {
		_ = conn.Close()
		return nil, err
	}

	reader := bufio.NewReader(conn)
	resp, err := http.ReadResponse(reader, &http.Request{Method: http.MethodConnect})
	if err != nil {
		_ = conn.Close()
		return nil, fmt.Errorf("read runner tunnel response: %w", err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		body, _ := io.ReadAll(io.LimitReader(resp.Body, 4096))
		_ = conn.Close()
		return nil, fmt.Errorf("runner tunnel rejected CONNECT: status=%d body=%q", resp.StatusCode, strings.TrimSpace(string(body)))
	}

	if err := conn.SetDeadline(time.Time{}); err != nil {
		_ = conn.Close()
		return nil, err
	}

	return &tunnelBufferedConn{Conn: conn, reader: reader}, nil
}

func dialRunnerControl(ctx context.Context, tunnelURL *url.URL) (net.Conn, error) {
	dialer := &net.Dialer{}
	switch tunnelURL.Scheme {
	case "http":
		return dialer.DialContext(ctx, "tcp", tunnelURL.Host)
	case "https":
		tlsDialer := &tls.Dialer{
			NetDialer: dialer,
			Config:    &tls.Config{ServerName: tunnelURL.Hostname()},
		}
		return tlsDialer.DialContext(ctx, "tcp", tunnelURL.Host)
	default:
		return nil, fmt.Errorf("unsupported runner tunnel scheme %q", tunnelURL.Scheme)
	}
}

func writeTunnelConnect(conn net.Conn, tunnelURL *url.URL, headers map[string]string) error {
	if _, err := fmt.Fprintf(conn, "CONNECT %s HTTP/1.1\r\nHost: %s\r\n", tunnelURL.RequestURI(), tunnelURL.Host); err != nil {
		return fmt.Errorf("write CONNECT request: %w", err)
	}
	for key, value := range headers {
		if _, err := fmt.Fprintf(conn, "%s: %s\r\n", key, value); err != nil {
			return fmt.Errorf("write CONNECT header: %w", err)
		}
	}
	if _, err := io.WriteString(conn, "\r\n"); err != nil {
		return fmt.Errorf("finish CONNECT request: %w", err)
	}
	return nil
}

type tunnelBufferedConn struct {
	net.Conn
	reader *bufio.Reader
}

func (c *tunnelBufferedConn) Read(p []byte) (int, error) {
	return c.reader.Read(p)
}

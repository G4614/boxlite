// Copyright 2026 BoxLite AI
// SPDX-License-Identifier: AGPL-3.0

package portgateway

import (
	"bufio"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
)

func TestServerRelaysAuthenticatedConnect(t *testing.T) {
	socketPath, serverErrs := startFakeGvproxyTunnel(t, 8080, func(conn net.Conn, reader *bufio.Reader) error {
		gotRequest, err := reader.ReadString('\n')
		if err != nil {
			return fmt.Errorf("read tunneled request line: %w", err)
		}
		if gotRequest != "GET / HTTP/1.1\r\n" {
			return fmt.Errorf("tunneled request line = %q", gotRequest)
		}

		_, err = conn.Write([]byte("HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok"))
		return err
	})

	server := httptest.NewServer(NewServer(ServerConfig{
		Logger:   testLogger(),
		ApiToken: "runner-token",
		Resolver: staticResolver(socketPath),
	}).Handler())
	defer server.Close()

	conn, reader := dialPortGateway(t, server.URL, "/boxes/box/tunnel/ports/8080", "runner-token")
	defer conn.Close()

	if _, err := fmt.Fprint(conn, "GET / HTTP/1.1\r\nHost: 8080-box.proxy.dev\r\n\r\n"); err != nil {
		t.Fatalf("write tunneled request: %v", err)
	}
	resp, err := http.ReadResponse(reader, nil)
	if err != nil {
		t.Fatalf("read tunneled response: %v", err)
	}
	defer resp.Body.Close()
	body, err := io.ReadAll(resp.Body)
	if err != nil {
		t.Fatalf("read response body: %v", err)
	}
	if resp.StatusCode != http.StatusOK || string(body) != "ok" {
		t.Fatalf("response = (%d, %q), want 200 ok", resp.StatusCode, body)
	}
	assertNoServerError(t, serverErrs)
}

func TestServerRejectsUnauthenticatedConnect(t *testing.T) {
	server := httptest.NewServer(NewServer(ServerConfig{
		Logger:   testLogger(),
		ApiToken: "runner-token",
		Resolver: staticResolver("/missing.sock"),
	}).Handler())
	defer server.Close()

	addr := strings.TrimPrefix(server.URL, "http://")
	conn, err := net.Dial("tcp", addr)
	if err != nil {
		t.Fatalf("dial port gateway: %v", err)
	}
	defer conn.Close()

	if _, err := fmt.Fprintf(conn, "CONNECT /boxes/box/tunnel/ports/8080 HTTP/1.1\r\nHost: %s\r\n\r\n", addr); err != nil {
		t.Fatalf("write CONNECT: %v", err)
	}
	resp, err := http.ReadResponse(bufio.NewReader(conn), &http.Request{Method: http.MethodConnect})
	if err != nil {
		t.Fatalf("read CONNECT response: %v", err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusUnauthorized {
		t.Fatalf("CONNECT status = %d, want 401", resp.StatusCode)
	}
}

func TestParseTunnelPath(t *testing.T) {
	boxID, port, err := parseTunnelPath("/boxes/box/tunnel/ports/8080")
	if err != nil {
		t.Fatalf("parseTunnelPath: %v", err)
	}
	if boxID != "box" || port != 8080 {
		t.Fatalf("parseTunnelPath = (%q, %d), want (box, 8080)", boxID, port)
	}

	for _, path := range []string{
		"/boxes//tunnel/ports/8080",
		"/boxes/box/tunnel/ports/0",
		"/boxes/box/tunnel/ports/65536",
		"/boxes/box/tunnel/ports/8080/extra",
		"/wrong/box/tunnel/ports/8080",
	} {
		if _, _, err := parseTunnelPath(path); err == nil {
			t.Fatalf("parseTunnelPath(%q) succeeded, want error", path)
		}
	}
}

func dialPortGateway(t *testing.T, serverURL string, path string, token string) (net.Conn, *bufio.Reader) {
	t.Helper()

	addr := strings.TrimPrefix(serverURL, "http://")
	conn, err := net.Dial("tcp", addr)
	if err != nil {
		t.Fatalf("dial port gateway: %v", err)
	}

	if _, err := fmt.Fprintf(conn, "CONNECT %s HTTP/1.1\r\nHost: %s\r\nX-BoxLite-Authorization: Bearer %s\r\n\r\n", path, addr, token); err != nil {
		_ = conn.Close()
		t.Fatalf("write CONNECT: %v", err)
	}

	reader := bufio.NewReader(conn)
	resp, err := http.ReadResponse(reader, &http.Request{Method: http.MethodConnect})
	if err != nil {
		_ = conn.Close()
		t.Fatalf("read CONNECT response: %v", err)
	}
	_ = resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		_ = conn.Close()
		t.Fatalf("CONNECT status = %d", resp.StatusCode)
	}

	return conn, reader
}

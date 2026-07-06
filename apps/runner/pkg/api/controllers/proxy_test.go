// Copyright 2026 BoxLite AI
// SPDX-License-Identifier: AGPL-3.0

package controllers

import (
	"bufio"
	"crypto/sha1"
	"encoding/base64"
	"fmt"
	"io"
	"log/slog"
	"net"
	"net/http"
	"net/http/httptest"
	"os"
	"strings"
	"testing"
	"time"
)

func TestIsTerminalToolboxPath(t *testing.T) {
	tests := []struct {
		path string
		want bool
	}{
		{"", true},
		{"/", true},
		{"proxy/22222", true},
		{"/proxy/22222", true},
		{"/proxy/22222/", true},
		{"/proxy/22222/vnc.html", true},
		{"/proxy/6080/", false},
		{"/computeruse/status", false},
		{"/process/execute", false},
	}

	for _, tt := range tests {
		t.Run(tt.path, func(t *testing.T) {
			if got := isTerminalToolboxPath(tt.path); got != tt.want {
				t.Fatalf("isTerminalToolboxPath(%q) = %v, want %v", tt.path, got, tt.want)
			}
		})
	}
}

func TestParseGuestPortProxyPath(t *testing.T) {
	tests := []struct {
		name     string
		path     string
		wantPort uint16
		wantPath string
		wantOK   bool
		wantErr  bool
	}{
		{name: "not proxy", path: "/computeruse/status", wantOK: false},
		{name: "port root", path: "/proxy/8080", wantPort: 8080, wantPath: "/", wantOK: true},
		{name: "port subpath", path: "/proxy/5173/assets/app.js", wantPort: 5173, wantPath: "/assets/app.js", wantOK: true},
		{name: "missing port", path: "/proxy/", wantOK: true, wantErr: true},
		{name: "bad port", path: "/proxy/nope", wantOK: true, wantErr: true},
		{name: "zero port", path: "/proxy/0", wantOK: true, wantErr: true},
		{name: "port too high", path: "/proxy/65536", wantOK: true, wantErr: true},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			gotPort, gotPath, gotOK, err := parseGuestPortProxyPath(tt.path)
			if gotOK != tt.wantOK {
				t.Fatalf("ok = %v, want %v", gotOK, tt.wantOK)
			}
			if (err != nil) != tt.wantErr {
				t.Fatalf("err = %v, wantErr %v", err, tt.wantErr)
			}
			if err != nil {
				return
			}
			if gotPort != tt.wantPort || gotPath != tt.wantPath {
				t.Fatalf("parseGuestPortProxyPath(%q) = (%d, %q), want (%d, %q)", tt.path, gotPort, gotPath, tt.wantPort, tt.wantPath)
			}
		})
	}
}

func TestGuestPortTunnelRelaysOpaqueHTTPViaIngress(t *testing.T) {
	socketPath, serverErrs := startFakeIngress(t, func(conn net.Conn, reader *bufio.Reader) error {
		line, err := reader.ReadString('\n')
		if err != nil {
			return fmt.Errorf("read ingress request: %w", err)
		}
		if line != "CONNECT /ports/8080 HTTP/1.1\r\n" {
			return fmt.Errorf("ingress request = %q", line)
		}
		if _, err := conn.Write([]byte("OK\n")); err != nil {
			return fmt.Errorf("write ingress ok: %w", err)
		}

		gotRequest, err := reader.ReadString('\n')
		if err != nil {
			return fmt.Errorf("read tunneled request line: %w", err)
		}
		if gotRequest != "GET /hello?x=1 HTTP/1.1\r\n" {
			return fmt.Errorf("tunneled request line = %q", gotRequest)
		}

		_, err = conn.Write([]byte("HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok"))
		return err
	})

	proxyServer := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		serveGuestPortTunnel(w, r, socketPath, 8080, testLogger())
	}))
	defer proxyServer.Close()

	conn, reader := dialRunnerTunnel(t, proxyServer.URL)
	defer conn.Close()

	if _, err := fmt.Fprint(conn, "GET /hello?x=1 HTTP/1.1\r\nHost: 8080-box.proxy.dev\r\n\r\n"); err != nil {
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

func TestGuestPortTunnelRelaysOpaqueWebSocketViaIngress(t *testing.T) {
	socketPath, serverErrs := startFakeIngress(t, func(conn net.Conn, reader *bufio.Reader) error {
		line, err := reader.ReadString('\n')
		if err != nil {
			return fmt.Errorf("read ingress request: %w", err)
		}
		if line != "CONNECT /ports/8081 HTTP/1.1\r\n" {
			return fmt.Errorf("ingress request = %q", line)
		}
		if _, err := conn.Write([]byte("OK\n")); err != nil {
			return fmt.Errorf("write ingress ok: %w", err)
		}

		req, err := http.ReadRequest(reader)
		if err != nil {
			return fmt.Errorf("read websocket request: %w", err)
		}
		if !strings.EqualFold(req.Header.Get("Upgrade"), "websocket") {
			return fmt.Errorf("missing websocket upgrade header: %v", req.Header)
		}
		if req.URL.Path != "/ws" {
			return fmt.Errorf("websocket path = %q, want /ws", req.URL.Path)
		}

		accept := websocketAccept(req.Header.Get("Sec-Websocket-Key"))
		if _, err := fmt.Fprintf(conn, "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: %s\r\n\r\n", accept); err != nil {
			return fmt.Errorf("write websocket handshake: %w", err)
		}

		msg, err := readMaskedTextFrame(reader)
		if err != nil {
			return err
		}
		if msg != "ping" {
			return fmt.Errorf("websocket payload = %q, want ping", msg)
		}
		return writeTextFrame(conn, "pong")
	})

	proxyServer := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		serveGuestPortTunnel(w, r, socketPath, 8081, testLogger())
	}))
	defer proxyServer.Close()

	conn, reader := dialRunnerTunnel(t, proxyServer.URL)
	defer conn.Close()

	key := "dGhlIHNhbXBsZSBub25jZQ=="
	if _, err := fmt.Fprintf(conn, "GET /ws HTTP/1.1\r\nHost: 8081-box.proxy.dev\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: %s\r\n\r\n", key); err != nil {
		t.Fatalf("write websocket handshake: %v", err)
	}
	resp, err := http.ReadResponse(reader, &http.Request{Method: http.MethodGet})
	if err != nil {
		t.Fatalf("read websocket handshake: %v", err)
	}
	if resp.StatusCode != http.StatusSwitchingProtocols {
		t.Fatalf("websocket status = %d", resp.StatusCode)
	}
	if err := writeMaskedTextFrame(conn, "ping"); err != nil {
		t.Fatalf("write websocket message: %v", err)
	}
	payload, err := readTextFrame(reader)
	if err != nil {
		t.Fatalf("read websocket message: %v", err)
	}
	if payload != "pong" {
		t.Fatalf("websocket response = %q, want pong", payload)
	}

	assertNoServerError(t, serverErrs)
}

func dialRunnerTunnel(t *testing.T, serverURL string) (net.Conn, *bufio.Reader) {
	t.Helper()

	addr := strings.TrimPrefix(serverURL, "http://")
	conn, err := net.Dial("tcp", addr)
	if err != nil {
		t.Fatalf("dial tunnel server: %v", err)
	}

	if _, err := fmt.Fprintf(conn, "CONNECT /boxes/box/tunnel/ports/8080 HTTP/1.1\r\nHost: %s\r\n\r\n", addr); err != nil {
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

func startFakeIngress(t *testing.T, handle func(net.Conn, *bufio.Reader) error) (string, <-chan error) {
	t.Helper()

	socketPath := fmt.Sprintf("/tmp/boxlite-runner-proxy-%d-%d.sock", os.Getpid(), time.Now().UnixNano())
	_ = os.Remove(socketPath)
	listener, err := net.Listen("unix", socketPath)
	if err != nil {
		t.Fatalf("listen fake ingress: %v", err)
	}
	t.Cleanup(func() {
		_ = listener.Close()
		_ = os.Remove(socketPath)
	})

	errs := make(chan error, 1)
	go func() {
		conn, err := listener.Accept()
		if err != nil {
			errs <- err
			return
		}
		defer conn.Close()
		_ = conn.SetDeadline(time.Now().Add(5 * time.Second))
		errs <- handle(conn, bufio.NewReader(conn))
	}()

	return socketPath, errs
}

func assertNoServerError(t *testing.T, errs <-chan error) {
	t.Helper()

	select {
	case err := <-errs:
		if err != nil {
			t.Fatalf("fake ingress error: %v", err)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("timed out waiting for fake ingress")
	}
}

func testLogger() *slog.Logger {
	return slog.New(slog.NewTextHandler(io.Discard, nil))
}

func websocketAccept(key string) string {
	const magic = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
	sum := sha1.Sum([]byte(key + magic))
	return base64.StdEncoding.EncodeToString(sum[:])
}

func readMaskedTextFrame(r *bufio.Reader) (string, error) {
	first, err := r.ReadByte()
	if err != nil {
		return "", fmt.Errorf("read websocket first byte: %w", err)
	}
	if first&0x0f != 0x1 {
		return "", fmt.Errorf("unexpected websocket opcode 0x%x", first&0x0f)
	}

	second, err := r.ReadByte()
	if err != nil {
		return "", fmt.Errorf("read websocket second byte: %w", err)
	}
	if second&0x80 == 0 {
		return "", fmt.Errorf("client websocket frame is not masked")
	}

	payloadLen := int(second & 0x7f)
	if payloadLen >= 126 {
		return "", fmt.Errorf("test frame too large")
	}

	mask := make([]byte, 4)
	if _, err := io.ReadFull(r, mask); err != nil {
		return "", fmt.Errorf("read websocket mask: %w", err)
	}

	payload := make([]byte, payloadLen)
	if _, err := io.ReadFull(r, payload); err != nil {
		return "", fmt.Errorf("read websocket payload: %w", err)
	}
	for i := range payload {
		payload[i] ^= mask[i%4]
	}
	return string(payload), nil
}

func writeTextFrame(w io.Writer, payload string) error {
	if len(payload) > 125 {
		return fmt.Errorf("payload too large")
	}
	_, err := w.Write(append([]byte{0x81, byte(len(payload))}, []byte(payload)...))
	return err
}

func writeMaskedTextFrame(w io.Writer, payload string) error {
	if len(payload) > 125 {
		return fmt.Errorf("payload too large")
	}
	mask := []byte{1, 2, 3, 4}
	frame := []byte{0x81, 0x80 | byte(len(payload))}
	frame = append(frame, mask...)
	for i, b := range []byte(payload) {
		frame = append(frame, b^mask[i%4])
	}
	_, err := w.Write(frame)
	return err
}

func readTextFrame(r *bufio.Reader) (string, error) {
	first, err := r.ReadByte()
	if err != nil {
		return "", fmt.Errorf("read websocket first byte: %w", err)
	}
	if first&0x0f != 0x1 {
		return "", fmt.Errorf("unexpected websocket opcode 0x%x", first&0x0f)
	}

	second, err := r.ReadByte()
	if err != nil {
		return "", fmt.Errorf("read websocket second byte: %w", err)
	}
	payloadLen := int(second & 0x7f)
	if payloadLen >= 126 {
		return "", fmt.Errorf("test frame too large")
	}

	payload := make([]byte, payloadLen)
	if _, err := io.ReadFull(r, payload); err != nil {
		return "", fmt.Errorf("read websocket payload: %w", err)
	}
	return string(payload), nil
}

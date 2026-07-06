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

	"github.com/gorilla/websocket"
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

func TestGuestPortReverseProxyRelaysHTTPViaIngress(t *testing.T) {
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

		req, err := http.ReadRequest(reader)
		if err != nil {
			return fmt.Errorf("read proxied request: %w", err)
		}
		if req.URL.Path != "/hello" {
			return fmt.Errorf("proxied path = %q, want /hello", req.URL.Path)
		}
		if req.URL.RawQuery != "x=1" {
			return fmt.Errorf("proxied query = %q, want x=1", req.URL.RawQuery)
		}
		if req.Host != "127.0.0.1:8080" {
			return fmt.Errorf("proxied host = %q, want 127.0.0.1:8080", req.Host)
		}
		if req.Header.Get("X-Forwarded-Host") != "8080-box.proxy.dev" {
			return fmt.Errorf("x-forwarded-host = %q", req.Header.Get("X-Forwarded-Host"))
		}

		_, err = conn.Write([]byte("HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok"))
		return err
	})

	req := httptest.NewRequest(http.MethodGet, "http://8080-box.proxy.dev/boxes/box/toolbox/proxy/8080/hello?x=1", nil)
	req.Host = "8080-box.proxy.dev"
	rec := httptest.NewRecorder()

	newGuestPortReverseProxy(socketPath, 8080, "/hello", testLogger()).ServeHTTP(rec, req)

	if rec.Code != http.StatusOK {
		t.Fatalf("status = %d, body = %q", rec.Code, rec.Body.String())
	}
	if rec.Body.String() != "ok" {
		t.Fatalf("body = %q, want ok", rec.Body.String())
	}
	assertNoServerError(t, serverErrs)
}

func TestGuestPortReverseProxyRelaysWebSocketViaIngress(t *testing.T) {
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
		newGuestPortReverseProxy(socketPath, 8081, "/ws", testLogger()).ServeHTTP(w, r)
	}))
	defer proxyServer.Close()

	wsURL := "ws" + strings.TrimPrefix(proxyServer.URL, "http") + "/boxes/box/toolbox/proxy/8081/ws"
	conn, _, err := websocket.DefaultDialer.Dial(wsURL, nil)
	if err != nil {
		t.Fatalf("dial websocket proxy: %v", err)
	}
	defer conn.Close()

	if err := conn.WriteMessage(websocket.TextMessage, []byte("ping")); err != nil {
		t.Fatalf("write websocket message: %v", err)
	}
	mt, payload, err := conn.ReadMessage()
	if err != nil {
		t.Fatalf("read websocket message: %v", err)
	}
	if mt != websocket.TextMessage || string(payload) != "pong" {
		t.Fatalf("websocket response = (%d, %q), want text pong", mt, payload)
	}

	assertNoServerError(t, serverErrs)
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

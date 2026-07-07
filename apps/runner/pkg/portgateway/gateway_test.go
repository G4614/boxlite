// Copyright 2026 BoxLite AI
// SPDX-License-Identifier: AGPL-3.0

package portgateway

import (
	"bufio"
	"context"
	"crypto/sha1"
	"encoding/base64"
	"encoding/binary"
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

type staticResolver string

func (r staticResolver) GvproxyGuestConnectorSocketPath(context.Context, string) (string, error) {
	return string(r), nil
}

func TestGatewayRelaysOpaqueHTTPViaGuestConnector(t *testing.T) {
	socketPath, serverErrs := startFakeGuestConnector(t, func(conn net.Conn, reader *bufio.Reader) error {
		if err := expectGuestConnectorRequest(reader, 8080); err != nil {
			return err
		}
		if _, err := conn.Write([]byte("OK\n")); err != nil {
			return fmt.Errorf("write guest connector ok: %w", err)
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
		New(staticResolver(socketPath), testLogger()).ServeConnect(w, r, "box", 8080)
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

func TestGatewayRelaysOpaqueWebSocketViaGuestConnector(t *testing.T) {
	socketPath, serverErrs := startFakeGuestConnector(t, func(conn net.Conn, reader *bufio.Reader) error {
		if err := expectGuestConnectorRequest(reader, 8081); err != nil {
			return err
		}
		if _, err := conn.Write([]byte("OK\n")); err != nil {
			return fmt.Errorf("write guest connector ok: %w", err)
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
		New(staticResolver(socketPath), testLogger()).ServeConnect(w, r, "box", 8081)
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

func TestPumpPreservesHalfCloseAndCountsBytes(t *testing.T) {
	clientApp, pumpClient := newTCPConnPair(t)
	defer clientApp.Close()
	defer pumpClient.Close()
	pumpGuest, guestApp := newTCPConnPair(t)
	defer pumpGuest.Close()
	defer guestApp.Close()

	deadline := time.Now().Add(5 * time.Second)
	setDeadline(t, clientApp, deadline)
	setDeadline(t, pumpClient, deadline)
	setDeadline(t, pumpGuest, deadline)
	setDeadline(t, guestApp, deadline)

	resultCh := make(chan PumpResult, 1)
	go func() {
		resultCh <- Pump(context.Background(), pumpClient, pumpGuest)
	}()

	if _, err := clientApp.Write([]byte("client-to-guest")); err != nil {
		t.Fatalf("write client payload: %v", err)
	}
	closeWrite(t, clientApp)

	gotClientPayload, err := io.ReadAll(guestApp)
	if err != nil {
		t.Fatalf("read guest payload: %v", err)
	}
	if string(gotClientPayload) != "client-to-guest" {
		t.Fatalf("guest payload = %q", gotClientPayload)
	}

	if _, err := guestApp.Write([]byte("guest-to-client")); err != nil {
		t.Fatalf("write guest payload: %v", err)
	}
	closeWrite(t, guestApp)

	gotGuestPayload, err := io.ReadAll(clientApp)
	if err != nil {
		t.Fatalf("read client payload: %v", err)
	}
	if string(gotGuestPayload) != "guest-to-client" {
		t.Fatalf("client payload = %q", gotGuestPayload)
	}

	select {
	case result := <-resultCh:
		if result.ClientToGuestBytes != int64(len("client-to-guest")) {
			t.Fatalf("client-to-guest bytes = %d", result.ClientToGuestBytes)
		}
		if result.GuestToClientBytes != int64(len("guest-to-client")) {
			t.Fatalf("guest-to-client bytes = %d", result.GuestToClientBytes)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("Pump did not return after both sides half-closed")
	}
}

func TestPumpStopsOnContextCancel(t *testing.T) {
	clientApp, pumpClient := newTCPConnPair(t)
	defer clientApp.Close()
	defer pumpClient.Close()
	pumpGuest, guestApp := newTCPConnPair(t)
	defer pumpGuest.Close()
	defer guestApp.Close()

	ctx, cancel := context.WithCancel(context.Background())
	resultCh := make(chan PumpResult, 1)
	go func() {
		resultCh <- Pump(ctx, pumpClient, pumpGuest)
	}()

	cancel()

	select {
	case <-resultCh:
	case <-time.After(5 * time.Second):
		t.Fatal("Pump did not stop after context cancellation")
	}
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

func newTCPConnPair(t *testing.T) (*net.TCPConn, *net.TCPConn) {
	t.Helper()

	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatalf("listen tcp: %v", err)
	}
	t.Cleanup(func() { _ = listener.Close() })

	accepted := make(chan *net.TCPConn, 1)
	acceptErr := make(chan error, 1)
	go func() {
		conn, err := listener.Accept()
		if err != nil {
			acceptErr <- err
			return
		}
		accepted <- conn.(*net.TCPConn)
	}()

	dialed, err := net.Dial("tcp", listener.Addr().String())
	if err != nil {
		t.Fatalf("dial tcp: %v", err)
	}

	select {
	case conn := <-accepted:
		return dialed.(*net.TCPConn), conn
	case err := <-acceptErr:
		_ = dialed.Close()
		t.Fatalf("accept tcp: %v", err)
	case <-time.After(5 * time.Second):
		_ = dialed.Close()
		t.Fatal("timed out accepting tcp connection")
	}

	panic("unreachable")
}

func setDeadline(t *testing.T, conn net.Conn, deadline time.Time) {
	t.Helper()
	if err := conn.SetDeadline(deadline); err != nil {
		t.Fatalf("set deadline: %v", err)
	}
}

func closeWrite(t *testing.T, conn *net.TCPConn) {
	t.Helper()
	if err := conn.CloseWrite(); err != nil {
		t.Fatalf("close write: %v", err)
	}
}

func startFakeGuestConnector(t *testing.T, handle func(net.Conn, *bufio.Reader) error) (string, <-chan error) {
	t.Helper()

	socketPath := fmt.Sprintf("/tmp/boxlite-runner-proxy-%d-%d.sock", os.Getpid(), time.Now().UnixNano())
	_ = os.Remove(socketPath)
	listener, err := net.Listen("unix", socketPath)
	if err != nil {
		t.Fatalf("listen fake guest connector: %v", err)
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
			t.Fatalf("fake guest connector error: %v", err)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("timed out waiting for fake guest connector")
	}
}

func expectGuestConnectorRequest(reader io.Reader, wantPort uint16) error {
	request := make([]byte, len(guestConnectorMagic)+2)
	if _, err := io.ReadFull(reader, request); err != nil {
		return fmt.Errorf("read guest connector request: %w", err)
	}
	if string(request[:len(guestConnectorMagic)]) != guestConnectorMagic {
		return fmt.Errorf("guest connector magic = %q, want %q", request[:len(guestConnectorMagic)], guestConnectorMagic)
	}
	if gotPort := binary.BigEndian.Uint16(request[len(guestConnectorMagic):]); gotPort != wantPort {
		return fmt.Errorf("guest connector port = %d, want %d", gotPort, wantPort)
	}
	return nil
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

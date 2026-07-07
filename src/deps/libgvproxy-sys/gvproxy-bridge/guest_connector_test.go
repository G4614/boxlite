package main

import (
	"bufio"
	"bytes"
	"context"
	"encoding/binary"
	"errors"
	"io"
	"net"
	"testing"
	"time"
)

func TestReadGuestConnectorRequest(t *testing.T) {
	req := guestConnectorRequest(8080)
	got, err := readGuestConnectorRequest(bufio.NewReader(reqReader(req)))
	if err != nil {
		t.Fatalf("readGuestConnectorRequest() error = %v", err)
	}
	if got != 8080 {
		t.Fatalf("readGuestConnectorRequest() = %d, want 8080", got)
	}
}

func TestReadGuestConnectorRequestRejectsInvalidInput(t *testing.T) {
	for _, req := range [][]byte{
		nil,
		[]byte("CONNECT /ports/8080 HTTP/1.1\r\n"),
		guestConnectorRequest(0),
	} {
		if got, err := readGuestConnectorRequest(reqReader(req)); err == nil {
			t.Fatalf("readGuestConnectorRequest(%q) = %d, want error", req, got)
		}
	}
}

func TestHandleGuestConnectorConnRelaysBothDirections(t *testing.T) {
	client, server := net.Pipe()
	guestClient, guestServer := net.Pipe()
	defer client.Close()
	defer guestServer.Close()

	deadline := time.Now().Add(5 * time.Second)
	_ = client.SetDeadline(deadline)
	_ = guestServer.SetDeadline(deadline)

	dialedPort := make(chan uint16, 1)
	done := make(chan struct{})
	go func() {
		defer close(done)
		handleGuestConnectorConn(context.Background(), server, func(_ context.Context, port uint16) (net.Conn, error) {
			dialedPort <- port
			return guestClient, nil
		})
	}()

	clientReader := bufio.NewReader(client)
	if _, err := client.Write(guestConnectorRequest(8080)); err != nil {
		t.Fatalf("write guest connector request: %v", err)
	}

	line, err := clientReader.ReadString('\n')
	if err != nil {
		t.Fatalf("read guest connector response: %v", err)
	}
	if line != "OK\n" {
		t.Fatalf("guest connector response = %q, want OK", line)
	}

	select {
	case got := <-dialedPort:
		if got != 8080 {
			t.Fatalf("dialed port = %d, want 8080", got)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("timed out waiting for dialed port")
	}

	writeClient := asyncWrite(client, "from-client")
	readExact(t, guestServer, "from-client")
	if err := <-writeClient; err != nil {
		t.Fatalf("write client payload: %v", err)
	}

	writeGuest := asyncWrite(guestServer, "from-guest")
	readExact(t, clientReader, "from-guest")
	if err := <-writeGuest; err != nil {
		t.Fatalf("write guest payload: %v", err)
	}

	_ = client.Close()
	_ = guestServer.Close()

	select {
	case <-done:
	case <-time.After(5 * time.Second):
		t.Fatal("guest connector handler did not exit after both pipes closed")
	}
}

func TestHandleGuestConnectorConnReportsDialError(t *testing.T) {
	client, server := net.Pipe()
	defer client.Close()

	_ = client.SetDeadline(time.Now().Add(5 * time.Second))

	done := make(chan struct{})
	go func() {
		defer close(done)
		handleGuestConnectorConn(context.Background(), server, func(context.Context, uint16) (net.Conn, error) {
			return nil, errors.New("guest refused")
		})
	}()

	reader := bufio.NewReader(client)
	if _, err := client.Write(guestConnectorRequest(8080)); err != nil {
		t.Fatalf("write guest connector request: %v", err)
	}

	line, err := reader.ReadString('\n')
	if err != nil {
		t.Fatalf("read guest connector response: %v", err)
	}
	if line != "ERR guest refused\n" {
		t.Fatalf("guest connector error response = %q, want guest refused", line)
	}

	select {
	case <-done:
	case <-time.After(5 * time.Second):
		t.Fatal("guest connector handler did not exit after dial error")
	}
}

func guestConnectorRequest(port uint16) []byte {
	req := make([]byte, len(guestConnectorMagic)+2)
	copy(req, guestConnectorMagic)
	binary.BigEndian.PutUint16(req[len(guestConnectorMagic):], port)
	return req
}

func reqReader(req []byte) io.Reader {
	return bytes.NewReader(req)
}

func asyncWrite(conn net.Conn, payload string) <-chan error {
	done := make(chan error, 1)
	go func() {
		_, err := conn.Write([]byte(payload))
		done <- err
	}()
	return done
}

func readExact(t *testing.T, reader io.Reader, want string) {
	t.Helper()
	buf := make([]byte, len(want))
	if _, err := io.ReadFull(reader, buf); err != nil {
		t.Fatalf("read payload: %v", err)
	}
	if got := string(buf); got != want {
		t.Fatalf("payload = %q, want %q", got, want)
	}
}

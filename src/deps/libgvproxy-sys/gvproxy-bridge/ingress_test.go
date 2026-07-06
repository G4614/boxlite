package main

import (
	"bufio"
	"context"
	"errors"
	"io"
	"net"
	"strings"
	"testing"
	"time"
)

func TestParseIngressPort(t *testing.T) {
	tests := []struct {
		name string
		line string
		want uint16
	}{
		{name: "plain port", line: "8080\n", want: 8080},
		{name: "connect port", line: "CONNECT 3000\n", want: 3000},
		{name: "connect path", line: "CONNECT /ports/5173 HTTP/1.1\r\n", want: 5173},
		{name: "connect host port", line: "CONNECT 127.0.0.1:9090 HTTP/1.1\r\n", want: 9090},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			got, err := parseIngressPort(tt.line)
			if err != nil {
				t.Fatalf("parseIngressPort() error = %v", err)
			}
			if got != tt.want {
				t.Fatalf("parseIngressPort() = %d, want %d", got, tt.want)
			}
		})
	}
}

func TestParseIngressPortRejectsInvalidInput(t *testing.T) {
	for _, line := range []string{"", "GET / HTTP/1.1\r\n", "CONNECT nope\n", "0\n", "65536\n"} {
		if got, err := parseIngressPort(line); err == nil {
			t.Fatalf("parseIngressPort(%q) = %d, want error", line, got)
		}
	}
}

func TestHandleIngressConnRelaysBothDirections(t *testing.T) {
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
		handleIngressConn(context.Background(), server, func(_ context.Context, port uint16) (net.Conn, error) {
			dialedPort <- port
			return guestClient, nil
		})
	}()

	clientReader := bufio.NewReader(client)
	if _, err := client.Write([]byte("CONNECT /ports/8080 HTTP/1.1\r\n")); err != nil {
		t.Fatalf("write ingress request: %v", err)
	}

	line, err := clientReader.ReadString('\n')
	if err != nil {
		t.Fatalf("read ingress response: %v", err)
	}
	if line != "OK\n" {
		t.Fatalf("ingress response = %q, want OK", line)
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
		t.Fatal("ingress handler did not exit after both pipes closed")
	}
}

func TestHandleIngressConnReportsDialError(t *testing.T) {
	client, server := net.Pipe()
	defer client.Close()

	_ = client.SetDeadline(time.Now().Add(5 * time.Second))

	done := make(chan struct{})
	go func() {
		defer close(done)
		handleIngressConn(context.Background(), server, func(context.Context, uint16) (net.Conn, error) {
			return nil, errors.New("guest refused")
		})
	}()

	reader := bufio.NewReader(client)
	if _, err := client.Write([]byte("CONNECT 8080\n")); err != nil {
		t.Fatalf("write ingress request: %v", err)
	}

	line, err := reader.ReadString('\n')
	if err != nil {
		t.Fatalf("read ingress response: %v", err)
	}
	if !strings.Contains(line, "ERR guest refused") {
		t.Fatalf("ingress error response = %q, want guest refused", line)
	}

	select {
	case <-done:
	case <-time.After(5 * time.Second):
		t.Fatal("ingress handler did not exit after dial error")
	}
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

package main

import (
	"context"
	"io"
	"net"
	"testing"
	"time"

	"github.com/inetaf/tcpproxy"
)

func TestTunnelProxyPreservesResponseAfterClientCloseWrite(t *testing.T) {
	backend, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	defer backend.Close()

	backendDone := make(chan error, 1)
	go func() {
		conn, err := backend.Accept()
		if err != nil {
			backendDone <- err
			return
		}
		defer conn.Close()

		request := make([]byte, 4)
		if _, err := io.ReadFull(conn, request); err != nil {
			backendDone <- err
			return
		}
		time.Sleep(50 * time.Millisecond)
		_, err = conn.Write([]byte("pong"))
		backendDone <- err
	}()

	frontend, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	defer frontend.Close()

	proxyDone := make(chan struct{})
	go func() {
		defer close(proxyDone)
		conn, err := frontend.Accept()
		if err != nil {
			return
		}
		proxy := tcpproxy.DialProxy{
			DialContext: func(ctx context.Context, network, _ string) (net.Conn, error) {
				var dialer net.Dialer
				return dialer.DialContext(ctx, network, backend.Addr().String())
			},
		}
		proxy.HandleConn(conn)
	}()

	clientConn, err := net.Dial("tcp", frontend.Addr().String())
	if err != nil {
		t.Fatal(err)
	}
	client := clientConn.(*net.TCPConn)
	defer client.Close()

	if _, err := client.Write([]byte("ping")); err != nil {
		t.Fatal(err)
	}
	if err := client.CloseWrite(); err != nil {
		t.Fatal(err)
	}
	response, err := io.ReadAll(client)
	if err != nil {
		t.Fatal(err)
	}
	if string(response) != "pong" {
		t.Fatalf("response after CloseWrite = %q, want %q", response, "pong")
	}
	if err := <-backendDone; err != nil {
		t.Fatal(err)
	}
	<-proxyDone
}

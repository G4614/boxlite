package main

import (
	"context"
	"encoding/binary"
	"fmt"
	"io"
	"net"
	"sync"
)

const guestConnectorMagic = "BLGC1"

type guestPortDialer func(context.Context, uint16) (net.Conn, error)

func handleGuestConnectorConn(ctx context.Context, client net.Conn, dial guestPortDialer) {
	defer client.Close()
	done := make(chan struct{})
	defer close(done)

	go func() {
		select {
		case <-ctx.Done():
			_ = client.Close()
		case <-done:
		}
	}()

	port, err := readGuestConnectorRequest(client)
	if err != nil {
		writeGuestConnectorError(client, err)
		return
	}

	guest, err := dial(ctx, port)
	if err != nil {
		writeGuestConnectorError(client, err)
		return
	}
	defer guest.Close()
	go func() {
		select {
		case <-ctx.Done():
			_ = guest.Close()
		case <-done:
		}
	}()

	if _, err := client.Write([]byte("OK\n")); err != nil {
		return
	}

	bridgeGuestConnectorConns(client, guest)
}

func readGuestConnectorRequest(reader io.Reader) (uint16, error) {
	request := make([]byte, len(guestConnectorMagic)+2)
	if _, err := io.ReadFull(reader, request); err != nil {
		return 0, fmt.Errorf("read guest connector request: %w", err)
	}

	if string(request[:len(guestConnectorMagic)]) != guestConnectorMagic {
		return 0, fmt.Errorf("invalid guest connector request")
	}

	port := binary.BigEndian.Uint16(request[len(guestConnectorMagic):])
	if port == 0 {
		return 0, fmt.Errorf("guest port must be in range 1-65535")
	}

	return port, nil
}

func writeGuestConnectorError(conn net.Conn, err error) {
	_, _ = fmt.Fprintf(conn, "ERR %s\n", err)
}

func bridgeGuestConnectorConns(client net.Conn, guest net.Conn) {
	var wg sync.WaitGroup
	wg.Add(2)

	go func() {
		defer wg.Done()
		_, _ = io.Copy(guest, client)
		closeWriteOrClose(guest)
	}()

	go func() {
		defer wg.Done()
		_, _ = io.Copy(client, guest)
		closeWriteOrClose(client)
	}()

	wg.Wait()
}

func closeWriteOrClose(conn net.Conn) {
	if closeWriter, ok := conn.(interface{ CloseWrite() error }); ok {
		_ = closeWriter.CloseWrite()
		return
	}
	_ = conn.Close()
}

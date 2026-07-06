package main

import (
	"bufio"
	"context"
	"errors"
	"fmt"
	"io"
	"net"
	"strconv"
	"strings"
	"sync"
)

const ingressMaxRequestLine = 128

type guestPortDialer func(context.Context, uint16) (net.Conn, error)

func handleIngressConn(ctx context.Context, client net.Conn, dial guestPortDialer) {
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

	reader := bufio.NewReaderSize(client, ingressMaxRequestLine)
	port, err := readIngressPort(reader)
	if err != nil {
		writeIngressError(client, err)
		return
	}

	guest, err := dial(ctx, port)
	if err != nil {
		writeIngressError(client, err)
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

	bridgeIngressConns(&ingressBufferedConn{Conn: client, reader: reader}, guest)
}

func readIngressPort(reader *bufio.Reader) (uint16, error) {
	line, err := reader.ReadSlice('\n')
	if errors.Is(err, bufio.ErrBufferFull) {
		return 0, fmt.Errorf("ingress request line too long")
	}
	if err != nil {
		return 0, fmt.Errorf("read ingress request: %w", err)
	}
	if len(line) > ingressMaxRequestLine {
		return 0, fmt.Errorf("ingress request line too long")
	}

	return parseIngressPort(string(line))
}

func parseIngressPort(line string) (uint16, error) {
	fields := strings.Fields(strings.TrimSpace(line))
	if len(fields) == 0 {
		return 0, errors.New("empty ingress request")
	}

	var rawPort string
	switch {
	case len(fields) == 1:
		rawPort = fields[0]
	case strings.EqualFold(fields[0], "CONNECT"):
		rawPort = fields[1]
	default:
		return 0, fmt.Errorf("unsupported ingress request %q", line)
	}

	rawPort = strings.Trim(rawPort, "/")
	rawPort = strings.TrimPrefix(rawPort, "ports/")
	if host, port, err := net.SplitHostPort(rawPort); err == nil {
		_ = host
		rawPort = port
	} else if idx := strings.LastIndex(rawPort, ":"); idx >= 0 {
		rawPort = rawPort[idx+1:]
	}

	port, err := strconv.ParseUint(rawPort, 10, 16)
	if err != nil || port == 0 {
		return 0, fmt.Errorf("invalid ingress port %q", rawPort)
	}

	return uint16(port), nil
}

func writeIngressError(conn net.Conn, err error) {
	_, _ = fmt.Fprintf(conn, "ERR %s\n", err)
}

func bridgeIngressConns(client net.Conn, guest net.Conn) {
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

type ingressBufferedConn struct {
	net.Conn
	reader io.Reader
}

func (c *ingressBufferedConn) Read(p []byte) (int, error) {
	return c.reader.Read(p)
}

package main

import (
	"context"
	"errors"
	"fmt"
	"net"
	"os"
	"time"

	logrus "github.com/sirupsen/logrus"
)

const guestConnectorDialTimeout = 10 * time.Second

func (i *GvproxyInstance) startGuestConnector(ctx context.Context, path string) error {
	if path == "" {
		return nil
	}

	if err := os.Remove(path); err != nil && !os.IsNotExist(err) {
		return fmt.Errorf("remove stale guest connector socket %q: %w", path, err)
	}

	listener, err := net.Listen("unix", path)
	if err != nil {
		return fmt.Errorf("listen guest connector socket %q: %w", path, err)
	}

	i.guestConnectorListener = listener
	i.guestConnectorWg.Add(1)
	go func() {
		defer i.guestConnectorWg.Done()
		i.acceptGuestConnector(ctx, listener)
	}()

	go func() {
		<-ctx.Done()
		_ = listener.Close()
	}()

	logrus.WithFields(logrus.Fields{
		"id":   i.ID,
		"path": path,
	}).Info("gvproxy guest connector listening")
	return nil
}

func (i *GvproxyInstance) acceptGuestConnector(ctx context.Context, listener net.Listener) {
	for {
		conn, err := listener.Accept()
		if err != nil {
			if ctx.Err() == nil {
				logrus.WithFields(logrus.Fields{
					"id":    i.ID,
					"error": err,
				}).Warn("gvproxy guest connector accept failed")
			}
			return
		}

		i.guestConnectorWg.Add(1)
		go func() {
			defer i.guestConnectorWg.Done()
			handleGuestConnectorConn(ctx, conn, i.dialGuestPort)
		}()
	}
}

func (i *GvproxyInstance) closeGuestConnector() {
	if i.guestConnectorListener != nil {
		_ = i.guestConnectorListener.Close()
	}
	i.guestConnectorWg.Wait()
	if i.GuestConnectorSocketPath != "" {
		_ = os.Remove(i.GuestConnectorSocketPath)
	}
}

func (i *GvproxyInstance) dialGuestPort(ctx context.Context, port uint16) (net.Conn, error) {
	i.vnMu.RLock()
	vn := i.vn
	guestIP := i.GuestIP
	i.vnMu.RUnlock()

	if vn == nil {
		return nil, errors.New("virtual network is not initialized")
	}
	if guestIP == "" {
		return nil, errors.New("guest IP is not configured")
	}

	dialCtx, cancel := context.WithTimeout(ctx, guestConnectorDialTimeout)
	defer cancel()

	return vn.DialContextTCP(dialCtx, fmt.Sprintf("%s:%d", guestIP, port))
}

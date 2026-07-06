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

const ingressDialTimeout = 10 * time.Second

func (i *GvproxyInstance) startIngress(ctx context.Context, path string) error {
	if path == "" {
		return nil
	}

	if err := os.Remove(path); err != nil && !os.IsNotExist(err) {
		return fmt.Errorf("remove stale ingress socket %q: %w", path, err)
	}

	listener, err := net.Listen("unix", path)
	if err != nil {
		return fmt.Errorf("listen ingress socket %q: %w", path, err)
	}

	i.ingressListener = listener
	i.ingressWg.Add(1)
	go func() {
		defer i.ingressWg.Done()
		i.acceptIngress(ctx, listener)
	}()

	go func() {
		<-ctx.Done()
		_ = listener.Close()
	}()

	logrus.WithFields(logrus.Fields{
		"id":   i.ID,
		"path": path,
	}).Info("gvproxy ingress listening")
	return nil
}

func (i *GvproxyInstance) acceptIngress(ctx context.Context, listener net.Listener) {
	for {
		conn, err := listener.Accept()
		if err != nil {
			if ctx.Err() == nil {
				logrus.WithFields(logrus.Fields{
					"id":    i.ID,
					"error": err,
				}).Warn("gvproxy ingress accept failed")
			}
			return
		}

		i.ingressWg.Add(1)
		go func() {
			defer i.ingressWg.Done()
			handleIngressConn(ctx, conn, i.dialGuestPort)
		}()
	}
}

func (i *GvproxyInstance) closeIngress() {
	if i.ingressListener != nil {
		_ = i.ingressListener.Close()
	}
	i.ingressWg.Wait()
	if i.IngressSocketPath != "" {
		_ = os.Remove(i.IngressSocketPath)
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

	dialCtx, cancel := context.WithTimeout(ctx, ingressDialTimeout)
	defer cancel()

	return vn.DialContextTCP(dialCtx, fmt.Sprintf("%s:%d", guestIP, port))
}

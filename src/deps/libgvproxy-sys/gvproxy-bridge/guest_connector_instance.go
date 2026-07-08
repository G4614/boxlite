package main

import (
	"context"
	"errors"
	"fmt"
	"net"
	"net/http"
	"os"

	logrus "github.com/sirupsen/logrus"
)

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

	i.vnMu.RLock()
	vn := i.vn
	i.vnMu.RUnlock()
	if vn == nil {
		_ = listener.Close()
		return errors.New("virtual network is not initialized")
	}

	server := &http.Server{
		Handler: vn.ServicesMux(),
	}

	i.guestConnectorListener = listener
	i.guestConnectorServer = server
	i.guestConnectorWg.Add(1)
	go func() {
		defer i.guestConnectorWg.Done()
		err := server.Serve(listener)
		if err != nil && !errors.Is(err, http.ErrServerClosed) && ctx.Err() == nil {
			logrus.WithFields(logrus.Fields{
				"id":    i.ID,
				"error": err,
			}).Warn("gvproxy services tunnel socket failed")
		}
	}()

	go func() {
		<-ctx.Done()
		_ = server.Close()
	}()

	logrus.WithFields(logrus.Fields{
		"id":   i.ID,
		"path": path,
	}).Info("gvproxy services tunnel socket listening")
	return nil
}

func (i *GvproxyInstance) closeGuestConnector() {
	if i.guestConnectorServer != nil {
		_ = i.guestConnectorServer.Close()
	}
	if i.guestConnectorListener != nil {
		_ = i.guestConnectorListener.Close()
	}
	i.guestConnectorWg.Wait()
	if i.GuestConnectorSocketPath != "" {
		_ = os.Remove(i.GuestConnectorSocketPath)
	}
}

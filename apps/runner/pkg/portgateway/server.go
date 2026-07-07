// Copyright 2026 BoxLite AI
// SPDX-License-Identifier: AGPL-3.0

package portgateway

import (
	"context"
	"errors"
	"fmt"
	"log/slog"
	"net"
	"net/http"
	"strconv"
	"strings"
	"time"

	"github.com/boxlite-ai/runner/internal/constants"
)

type ServerConfig struct {
	Logger   *slog.Logger
	Port     int
	ApiToken string
	Resolver ConnectorResolver
}

type Server struct {
	logger     *slog.Logger
	port       int
	apiToken   string
	gateway    *Gateway
	httpServer *http.Server
}

func NewServer(config ServerConfig) *Server {
	logger := config.Logger
	if logger == nil {
		logger = slog.Default()
	}
	return &Server{
		logger:   logger.With(slog.String("component", "port_gateway")),
		port:     config.Port,
		apiToken: config.ApiToken,
		gateway:  New(config.Resolver, logger),
	}
}

func (s *Server) Start(ctx context.Context) error {
	if s.port <= 0 {
		return errors.New("port gateway port must be greater than 0")
	}

	s.httpServer = &http.Server{
		Addr:    fmt.Sprintf(":%d", s.port),
		Handler: s.Handler(),
	}

	listener, err := net.Listen("tcp", s.httpServer.Addr)
	if err != nil {
		return err
	}

	go func() {
		<-ctx.Done()
		s.Stop()
	}()

	s.logger.InfoContext(ctx, "Starting runner port gateway", "port", s.port)
	err = s.httpServer.Serve(listener)
	if errors.Is(err, http.ErrServerClosed) {
		return nil
	}
	return err
}

func (s *Server) Stop() {
	if s.httpServer == nil {
		return
	}
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	if err := s.httpServer.Shutdown(ctx); err != nil {
		s.logger.Error("Failed to shutdown port gateway", "error", err)
	}
}

func (s *Server) Handler() http.Handler {
	mux := http.NewServeMux()
	mux.HandleFunc("/boxes/", s.handleConnect)
	return mux
}

func (s *Server) handleConnect(w http.ResponseWriter, req *http.Request) {
	if !s.authorized(req) {
		http.Error(w, "unauthorized", http.StatusUnauthorized)
		return
	}
	if req.Method != http.MethodConnect {
		http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
		return
	}

	boxID, port, err := parseTunnelPath(req.URL.Path)
	if err != nil {
		http.Error(w, err.Error(), http.StatusBadRequest)
		return
	}

	s.gateway.ServeConnect(w, req, boxID, port)
}

func (s *Server) authorized(req *http.Request) bool {
	authHeader := req.Header.Get(constants.BOXLITE_AUTHORIZATION_HEADER)
	if authHeader == "" {
		authHeader = req.Header.Get(constants.AUTHORIZATION_HEADER)
	}
	req.Header.Del(constants.BOXLITE_AUTHORIZATION_HEADER)

	parts := strings.Split(authHeader, " ")
	return len(parts) == 2 && parts[0] == constants.BEARER_AUTH_HEADER && parts[1] == s.apiToken
}

func parseTunnelPath(path string) (string, uint16, error) {
	rest, ok := strings.CutPrefix(path, "/boxes/")
	if !ok {
		return "", 0, fmt.Errorf("invalid tunnel path")
	}
	boxID, rest, ok := strings.Cut(rest, "/tunnel/ports/")
	if !ok || boxID == "" || rest == "" {
		return "", 0, fmt.Errorf("invalid tunnel path")
	}
	if strings.Contains(rest, "/") {
		return "", 0, fmt.Errorf("invalid tunnel path")
	}

	port64, err := strconv.ParseUint(rest, 10, 16)
	if err != nil || port64 == 0 {
		return "", 0, fmt.Errorf("invalid tunnel port %q", rest)
	}
	return boxID, uint16(port64), nil
}

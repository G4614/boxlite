// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 BoxLite AI

package boxlite

import (
	"log/slog"
	"testing"

	"github.com/boxlite-ai/runner/pkg/api/dto"
)

func TestPublishedPortWhitelist(t *testing.T) {
	client := &Client{
		logger:         slog.Default(),
		publishedPorts: make(map[string]map[int]int),
	}

	client.recordPublishedPortsLocked("box-1", portSetFromDTO([]dto.PortDTO{
		{GuestPort: 8000},
		{HostPort: 18080, GuestPort: 8080},
	}))

	hostPort, ok := client.PublishedHostPort("box-1", 8000)
	if !ok || hostPort != 8000 {
		t.Fatalf("guest-only port should publish the same host port, got host=%d ok=%v", hostPort, ok)
	}
	hostPort, ok = client.PublishedHostPort("box-1", 8080)
	if !ok || hostPort != 18080 {
		t.Fatalf("explicit mapping should publish guest port to host port, got host=%d ok=%v", hostPort, ok)
	}
	if _, ok := client.PublishedHostPort("box-1", 18080); ok {
		t.Fatal("host port must not be used as the public guest port")
	}
	if _, ok := client.PublishedHostPort("other-box", 8000); ok {
		t.Fatal("ports must be scoped by box")
	}
}

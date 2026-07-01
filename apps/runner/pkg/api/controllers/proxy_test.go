// Copyright 2026 BoxLite AI
// SPDX-License-Identifier: AGPL-3.0

package controllers

import (
	"testing"
)

func TestIsTerminalToolboxPath(t *testing.T) {
	tests := []struct {
		path string
		want bool
	}{
		{"", true},
		{"/", true},
		{"proxy/22222", true},
		{"/proxy/22222", true},
		{"/proxy/22222/", true},
		{"/proxy/22222/vnc.html", true},
		{"/proxy/6080/", false},
		{"/computeruse/status", false},
		{"/process/execute", false},
	}

	for _, tt := range tests {
		t.Run(tt.path, func(t *testing.T) {
			if got := isTerminalToolboxPath(tt.path); got != tt.want {
				t.Fatalf("isTerminalToolboxPath(%q) = %v, want %v", tt.path, got, tt.want)
			}
		})
	}
}

func TestPublicPortProxyTarget(t *testing.T) {
	targetPath, port, err := publicPortProxyTarget("/proxy/8080/hello/world")
	if err != nil {
		t.Fatalf("publicPortProxyTarget returned error: %v", err)
	}
	if port != 8080 {
		t.Fatalf("port = %d, want 8080", port)
	}

	if targetPath != "/hello/world" {
		t.Fatalf("targetPath = %q, want /hello/world", targetPath)
	}
	target := publicPortTargetURL(18080, targetPath)
	if target.String() != "http://127.0.0.1:18080/hello/world" {
		t.Fatalf("target = %q, want %q", target.String(), "http://127.0.0.1:18080/hello/world")
	}
}

// Copyright 2026 BoxLite AI
// SPDX-License-Identifier: AGPL-3.0

package controllers

import "testing"

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

func TestParseGuestPortProxyPath(t *testing.T) {
	tests := []struct {
		name     string
		path     string
		wantPort uint16
		wantPath string
		wantOK   bool
		wantErr  bool
	}{
		{name: "not proxy", path: "/computeruse/status", wantOK: false},
		{name: "port root", path: "/proxy/8080", wantPort: 8080, wantPath: "/", wantOK: true},
		{name: "port subpath", path: "/proxy/5173/assets/app.js", wantPort: 5173, wantPath: "/assets/app.js", wantOK: true},
		{name: "missing port", path: "/proxy/", wantOK: true, wantErr: true},
		{name: "bad port", path: "/proxy/nope", wantOK: true, wantErr: true},
		{name: "zero port", path: "/proxy/0", wantOK: true, wantErr: true},
		{name: "port too high", path: "/proxy/65536", wantOK: true, wantErr: true},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			gotPort, gotPath, gotOK, err := parseGuestPortProxyPath(tt.path)
			if gotOK != tt.wantOK {
				t.Fatalf("ok = %v, want %v", gotOK, tt.wantOK)
			}
			if (err != nil) != tt.wantErr {
				t.Fatalf("err = %v, wantErr %v", err, tt.wantErr)
			}
			if err != nil {
				return
			}
			if gotPort != tt.wantPort || gotPath != tt.wantPath {
				t.Fatalf("parseGuestPortProxyPath(%q) = (%d, %q), want (%d, %q)", tt.path, gotPort, gotPath, tt.wantPort, tt.wantPath)
			}
		})
	}
}

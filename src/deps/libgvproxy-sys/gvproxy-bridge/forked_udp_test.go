package main

import (
	"net"
	"testing"
)

// TestUDPAllowed is the security regression for finding #2: with allow_net set,
// UDP to non-allowlisted destinations (e.g. DNS-over-UDP exfiltration to an
// attacker on :53, or QUIC on :443) must be blocked, while allowlisted hosts and
// the gateway DNS resolver still pass. With no filter, all UDP is allowed
// (unchanged behavior).
func TestUDPAllowed(t *testing.T) {
	// allow_net = api allowlisted IP; internal/gateway IPs always allowed.
	f := NewTCPFilter([]string{"1.2.3.4"}, "192.168.127.1", "192.168.127.2", "192.168.127.254")

	cases := []struct {
		name string
		ip   string
		port uint16
		want bool
	}{
		{"allowlisted dest", "1.2.3.4", 443, true},
		{"gateway DNS", "192.168.127.1", 53, true},
		{"attacker DNS exfil blocked", "203.0.113.9", 53, false},
		{"attacker QUIC blocked", "203.0.113.9", 443, false},
		{"unlisted host blocked", "8.8.8.8", 12345, false},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			got := udpAllowed(net.ParseIP(tc.ip), tc.port, f)
			if got != tc.want {
				t.Errorf("udpAllowed(%s:%d) = %v, want %v", tc.ip, tc.port, got, tc.want)
			}
		})
	}

	// No filter: everything allowed (prior behavior).
	if !udpAllowed(net.ParseIP("203.0.113.9"), 53, nil) {
		t.Error("udpAllowed with nil filter should allow all UDP")
	}
}

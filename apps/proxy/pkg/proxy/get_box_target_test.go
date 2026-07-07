// Copyright 2026 BoxLite AI
// SPDX-License-Identifier: AGPL-3.0

package proxy

import (
	"testing"

	"github.com/boxlite-ai/proxy/cmd/proxy/config"
)

func TestRunnerPortGatewayURLUsesConfiguredDataPort(t *testing.T) {
	proxy := &Proxy{config: &config.Config{RunnerPortGatewayPort: 3004}}

	got, err := proxy.runnerPortGatewayURL("http://10.0.0.2:3003")
	if err != nil {
		t.Fatal(err)
	}
	if got != "http://10.0.0.2:3004" {
		t.Fatalf("runnerPortGatewayURL = %q, want http://10.0.0.2:3004", got)
	}
}

func TestRunnerPortGatewayURLDerivesDataPortFromRunnerAPIURL(t *testing.T) {
	proxy := &Proxy{config: &config.Config{}}

	got, err := proxy.runnerPortGatewayURL("http://10.0.0.2:3003/")
	if err != nil {
		t.Fatal(err)
	}
	if got != "http://10.0.0.2:3004" {
		t.Fatalf("runnerPortGatewayURL = %q, want http://10.0.0.2:3004", got)
	}
}

func TestRunnerPortGatewayURLFallsBackWhenRunnerAPIURLHasNoPort(t *testing.T) {
	proxy := &Proxy{config: &config.Config{}}

	got, err := proxy.runnerPortGatewayURL("https://runner.internal/")
	if err != nil {
		t.Fatal(err)
	}
	if got != "https://runner.internal" {
		t.Fatalf("runnerPortGatewayURL = %q, want https://runner.internal", got)
	}
}

// Copyright 2026 BoxLite AI
// SPDX-License-Identifier: AGPL-3.0

package proxy

import (
	"fmt"
	"io"
	"net/http"
	"net/http/httptest"
	"net/url"
	"testing"
)

func TestRunnerTunnelTransportRelaysHTTPOverConnect(t *testing.T) {
	done := make(chan error, 1)
	runner := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Method != http.MethodConnect {
			t.Errorf("method = %s, want CONNECT", r.Method)
		}
		if r.URL.Path != "/boxes/box/tunnel/ports/8080" {
			t.Errorf("path = %s", r.URL.Path)
		}
		if r.Header.Get("X-BoxLite-Authorization") != "Bearer runner-token" {
			t.Errorf("runner auth header = %q", r.Header.Get("X-BoxLite-Authorization"))
		}

		hijacker, ok := w.(http.Hijacker)
		if !ok {
			done <- fmt.Errorf("response writer does not support hijack")
			return
		}
		conn, rw, err := hijacker.Hijack()
		if err != nil {
			done <- err
			return
		}
		defer conn.Close()

		if _, err := rw.WriteString("HTTP/1.1 200 Connection Established\r\n\r\n"); err != nil {
			done <- err
			return
		}
		if err := rw.Flush(); err != nil {
			done <- err
			return
		}

		req, err := http.ReadRequest(rw.Reader)
		if err != nil {
			done <- err
			return
		}
		if req.URL.String() != "/hello?x=1" {
			done <- fmt.Errorf("guest request URL = %q", req.URL.String())
			return
		}
		if req.Host != "127.0.0.1:8080" {
			done <- fmt.Errorf("guest host = %q", req.Host)
			return
		}
		if req.Header.Get("X-Forwarded-Host") != "8080-box.proxy.dev" {
			done <- fmt.Errorf("x-forwarded-host = %q", req.Header.Get("X-Forwarded-Host"))
			return
		}
		if req.Header.Get("X-BoxLite-Authorization") != "" {
			done <- fmt.Errorf("runner auth leaked to guest")
			return
		}

		_, err = io.WriteString(conn, "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
		done <- err
	}))
	defer runner.Close()

	tunnelURL, err := url.Parse(runner.URL + "/boxes/box/tunnel/ports/8080")
	if err != nil {
		t.Fatal(err)
	}

	client := http.Client{
		Transport: newRunnerTunnelTransport(tunnelURL, map[string]string{
			"X-BoxLite-Authorization": "Bearer runner-token",
		}),
	}
	req, err := http.NewRequest(http.MethodGet, "http://127.0.0.1:8080/hello?x=1", nil)
	if err != nil {
		t.Fatal(err)
	}
	req.Host = "127.0.0.1:8080"
	req.Header.Set("X-Forwarded-Host", "8080-box.proxy.dev")

	resp, err := client.Do(req)
	if err != nil {
		t.Fatalf("Do: %v", err)
	}
	defer resp.Body.Close()

	body, err := io.ReadAll(resp.Body)
	if err != nil {
		t.Fatalf("ReadAll: %v", err)
	}
	if resp.StatusCode != http.StatusOK || string(body) != "ok" {
		t.Fatalf("response = (%d, %q), want 200 ok", resp.StatusCode, body)
	}

	if err := <-done; err != nil {
		t.Fatal(err)
	}
}

func TestRunnerTunnelTransportRejectsFailedConnect(t *testing.T) {
	runner := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		http.Error(w, "nope", http.StatusUnauthorized)
	}))
	defer runner.Close()

	tunnelURL, err := url.Parse(runner.URL + "/boxes/box/tunnel/ports/8080")
	if err != nil {
		t.Fatal(err)
	}

	conn, err := newRunnerTunnelTransport(tunnelURL, nil).(*runnerTunnelTransport).dialTunnel(t.Context())
	if err == nil {
		_ = conn.Close()
		t.Fatal("dialTunnel succeeded, want error")
	}
}

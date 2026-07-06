package main

import (
	"compress/gzip"
	"context"
	"crypto/tls"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"runtime"
	"strings"
	"sync"
	"testing"
	"time"

	"golang.org/x/net/http2"
	"golang.org/x/sync/errgroup"
)

// startTestUpstream starts a local HTTPS server that uses the provided handler.
// Returns the server, its address (host:port), and a cleanup function.
func startTestUpstream(t *testing.T, handler http.HandlerFunc) (addr string, cleanup func()) {
	t.Helper()
	srv := httptest.NewTLSServer(handler)
	// Extract host:port from the server URL (strip https://)
	addr = strings.TrimPrefix(srv.URL, "https://")
	return addr, srv.Close
}

// dialThroughMITM creates an in-memory pipe, starts mitmAndForward on the server side,
// and returns an http.Client configured to speak through the MITM proxy with the box CA trusted.
func dialThroughMITM(t *testing.T, ca *BoxCA, hostname, destAddr string, secrets []SecretConfig) *http.Client {
	t.Helper()
	return dialThroughMITMWithProto(t, ca, hostname, destAddr, secrets, "")
}

// dialThroughMITMH2 creates a client that forces HTTP/2 via h2.Transport.
func dialThroughMITMH2(t *testing.T, ca *BoxCA, hostname, destAddr string, secrets []SecretConfig) *http.Client {
	t.Helper()
	return dialThroughMITMWithProto(t, ca, hostname, destAddr, secrets, "h2")
}

func dialThroughMITMWithProto(t *testing.T, ca *BoxCA, hostname, destAddr string, secrets []SecretConfig, forceProto string) *http.Client {
	t.Helper()

	caPool, _ := ca.CACertPool()

	guest, proxy := net.Pipe()
	go mitmAndForward(proxy, hostname, destAddr, ca, secrets, &tls.Config{InsecureSkipVerify: true})

	nextProtos := []string{"http/1.1"}
	if forceProto == "h2" {
		nextProtos = []string{"h2", "http/1.1"}
	}

	tlsCfg := &tls.Config{
		ServerName:         hostname,
		RootCAs:            caPool,
		NextProtos:         nextProtos,
		InsecureSkipVerify: caPool == nil,
	}

	if forceProto == "h2" {
		// For HTTP/2: do TLS handshake once, then use http2.Transport
		// which natively multiplexes on a single connection.
		tlsConn := tls.Client(guest, tlsCfg)
		h2Transport := &http2.Transport{
			DialTLSContext: func(ctx context.Context, network, addr string, cfg *tls.Config) (net.Conn, error) {
				return tlsConn, nil
			},
			TLSClientConfig: tlsCfg,
		}
		return &http.Client{
			Transport: h2Transport,
			Timeout:   10 * time.Second,
		}
	}

	// For HTTP/1.1: single TLS connection over the pipe.
	// We do TLS once; the transport reuses it for keep-alive.
	var h1Once sync.Once
	var h1Conn net.Conn
	var h1Err error
	transport := &http.Transport{
		DialTLSContext: func(ctx context.Context, network, addr string) (net.Conn, error) {
			h1Once.Do(func() {
				h1Conn = tls.Client(guest, tlsCfg)
				// Don't handshake here — let transport do it
			})
			if h1Err != nil {
				return nil, h1Err
			}
			return h1Conn, nil
		},
	}

	return &http.Client{
		Transport: transport,
		Timeout:   10 * time.Second,
	}
}

func testSecrets() []SecretConfig {
	return []SecretConfig{
		{
			Name:        "k",
			Hosts:       []string{"api.example.com"},
			Placeholder: "<BOXLITE_SECRET:k>",
			Value:       "real-value",
		},
	}
}

// capturedUpstream records what the upstream actually received. secretTransport
// now scrubs reflected secret values out of responses, so an echoed response no
// longer reveals whether the request was substituted — tests assert on the
// captured request instead, and separately assert the response was scrubbed.
type capturedUpstream struct {
	mu     sync.Mutex
	auths  []string
	bodies []string
}

func (c *capturedUpstream) addAuth(a string) {
	c.mu.Lock()
	c.auths = append(c.auths, a)
	c.mu.Unlock()
}

func (c *capturedUpstream) addBody(b string) {
	c.mu.Lock()
	c.bodies = append(c.bodies, b)
	c.mu.Unlock()
}

func (c *capturedUpstream) snapshotAuths() []string {
	c.mu.Lock()
	defer c.mu.Unlock()
	return append([]string(nil), c.auths...)
}

func (c *capturedUpstream) snapshotBodies() []string {
	c.mu.Lock()
	defer c.mu.Unlock()
	return append([]string(nil), c.bodies...)
}

// --- HTTP/1.1 Tests ---

func TestMitmProxy_HTTP1_BasicRequest(t *testing.T) {
	ca := newTestCA(t)

	secrets := testSecrets()

	// Upstream records and echoes the Authorization header it received.
	var captured capturedUpstream
	addr, cleanup := startTestUpstream(t, func(w http.ResponseWriter, r *http.Request) {
		auth := r.Header.Get("Authorization")
		captured.addAuth(auth)
		fmt.Fprintf(w, `{"authorization":%q}`, auth)
	})
	defer cleanup()

	client := dialThroughMITM(t, ca, "api.example.com", addr, secrets)

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	req, err := http.NewRequestWithContext(ctx, "GET", "https://api.example.com/test", nil)
	if err != nil {
		t.Fatal(err)
	}
	req.Header.Set("Authorization", "Bearer <BOXLITE_SECRET:k>")

	resp, err := client.Do(req)
	if err != nil {
		t.Fatal("request failed:", err)
	}
	defer resp.Body.Close()

	body, err := io.ReadAll(resp.Body)
	if err != nil {
		t.Fatal("failed to read response:", err)
	}

	// Request leg: upstream must have received the substituted real value.
	auths := captured.snapshotAuths()
	if len(auths) != 1 || !strings.Contains(auths[0], "real-value") {
		t.Errorf("expected upstream to receive substituted value, got: %v", auths)
	}
	if len(auths) == 1 && strings.Contains(auths[0], "BOXLITE_SECRET") {
		t.Errorf("placeholder was not substituted in upstream request: %s", auths[0])
	}
	// Response leg: the reflected value must be scrubbed back to the placeholder.
	got := string(body)
	if strings.Contains(got, "real-value") {
		t.Errorf("secret value leaked back to guest in response: %s", got)
	}
	if !strings.Contains(got, "<BOXLITE_SECRET:k>") {
		t.Errorf("expected placeholder in scrubbed response, got: %s", got)
	}
}

func TestMitmProxy_HTTP1_PostWithBody(t *testing.T) {
	ca := newTestCA(t)

	secrets := testSecrets()

	// Upstream records and echoes the request body it received.
	var captured capturedUpstream
	addr, cleanup := startTestUpstream(t, func(w http.ResponseWriter, r *http.Request) {
		b, _ := io.ReadAll(r.Body)
		captured.addBody(string(b))
		w.Write(b)
	})
	defer cleanup()

	client := dialThroughMITM(t, ca, "api.example.com", addr, secrets)

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	bodyStr := `{"key":"<BOXLITE_SECRET:k>"}`
	req, err := http.NewRequestWithContext(ctx, "POST", "https://api.example.com/data", strings.NewReader(bodyStr))
	if err != nil {
		t.Fatal(err)
	}
	req.Header.Set("Content-Type", "application/json")

	resp, err := client.Do(req)
	if err != nil {
		t.Fatal("request failed:", err)
	}
	defer resp.Body.Close()

	body, err := io.ReadAll(resp.Body)
	if err != nil {
		t.Fatal("failed to read response:", err)
	}

	// Request leg: upstream must have received the substituted body.
	bodies := captured.snapshotBodies()
	if len(bodies) != 1 || bodies[0] != `{"key":"real-value"}` {
		t.Errorf("expected upstream to receive substituted body, got: %v", bodies)
	}
	// Response leg: the reflected value must be scrubbed back to the placeholder.
	got := string(body)
	if got != `{"key":"<BOXLITE_SECRET:k>"}` {
		t.Errorf("expected scrubbed response body, got %q", got)
	}
}

func TestMitmProxy_HTTP1_KeepAlive(t *testing.T) {
	ca := newTestCA(t)

	secrets := testSecrets()
	var captured capturedUpstream

	addr, cleanup := startTestUpstream(t, func(w http.ResponseWriter, r *http.Request) {
		auth := r.Header.Get("Authorization")
		captured.addAuth(auth)
		fmt.Fprintf(w, `{"auth":%q}`, auth)
	})
	defer cleanup()

	client := dialThroughMITM(t, ca, "api.example.com", addr, secrets)

	for i := 0; i < 5; i++ {
		ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)

		req, err := http.NewRequestWithContext(ctx, "GET", "https://api.example.com/test", nil)
		if err != nil {
			cancel()
			t.Fatal(err)
		}
		req.Header.Set("Authorization", "Bearer <BOXLITE_SECRET:k>")

		resp, err := client.Do(req)
		if err != nil {
			cancel()
			t.Fatalf("request %d failed: %v", i, err)
		}

		body, _ := io.ReadAll(resp.Body)
		resp.Body.Close()
		cancel()

		// Response leg: reflected value scrubbed back to the placeholder.
		got := string(body)
		if strings.Contains(got, "real-value") {
			t.Errorf("request %d: secret leaked back to guest: %s", i, got)
		}
		if !strings.Contains(got, "<BOXLITE_SECRET:k>") {
			t.Errorf("request %d: expected placeholder in scrubbed response, got: %s", i, got)
		}
	}

	// Request leg: all 5 requests reached upstream with the substituted value.
	auths := captured.snapshotAuths()
	if len(auths) != 5 {
		t.Errorf("expected 5 requests at upstream, got %d", len(auths))
	}
	for i, a := range auths {
		if !strings.Contains(a, "real-value") || strings.Contains(a, "BOXLITE_SECRET") {
			t.Errorf("request %d: upstream did not receive substituted value: %s", i, a)
		}
	}
}

// --- HTTP/2 Tests ---

func TestMitmProxy_HTTP2_BasicRequest(t *testing.T) {
	ca := newTestCA(t)

	secrets := testSecrets()

	var captured capturedUpstream
	addr, cleanup := startTestUpstream(t, func(w http.ResponseWriter, r *http.Request) {
		proto := r.Proto
		auth := r.Header.Get("Authorization")
		captured.addAuth(auth)
		fmt.Fprintf(w, `{"proto":%q,"auth":%q}`, proto, auth)
	})
	defer cleanup()

	client := dialThroughMITMH2(t, ca, "api.example.com", addr, secrets)

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	req, err := http.NewRequestWithContext(ctx, "GET", "https://api.example.com/test", nil)
	if err != nil {
		t.Fatal(err)
	}
	req.Header.Set("Authorization", "Bearer <BOXLITE_SECRET:k>")

	resp, err := client.Do(req)
	if err != nil {
		t.Fatal("request failed:", err)
	}
	defer resp.Body.Close()

	body, err := io.ReadAll(resp.Body)
	if err != nil {
		t.Fatal("failed to read response:", err)
	}

	// Request leg: upstream must have received the substituted real value.
	auths := captured.snapshotAuths()
	if len(auths) != 1 || !strings.Contains(auths[0], "real-value") {
		t.Errorf("expected upstream to receive substituted value, got: %v", auths)
	}
	// Response leg: the reflected value must be scrubbed back to the placeholder.
	got := string(body)
	if strings.Contains(got, "real-value") {
		t.Errorf("secret value leaked back to guest in response: %s", got)
	}
	if !strings.Contains(got, "<BOXLITE_SECRET:k>") {
		t.Errorf("expected placeholder in scrubbed response, got: %s", got)
	}
}

func TestMitmProxy_HTTP2_MultiplexedStreams(t *testing.T) {
	ca := newTestCA(t)

	secrets := testSecrets()

	var captured capturedUpstream
	addr, cleanup := startTestUpstream(t, func(w http.ResponseWriter, r *http.Request) {
		auth := r.Header.Get("Authorization")
		captured.addAuth(auth)
		fmt.Fprintf(w, `{"auth":%q}`, auth)
	})
	defer cleanup()

	client := dialThroughMITMH2(t, ca, "api.example.com", addr, secrets)

	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()

	g, ctx := errgroup.WithContext(ctx)

	for i := 0; i < 10; i++ {
		i := i
		g.Go(func() error {
			req, err := http.NewRequestWithContext(ctx, "GET", "https://api.example.com/test", nil)
			if err != nil {
				return fmt.Errorf("request %d: create failed: %w", i, err)
			}
			req.Header.Set("Authorization", "Bearer <BOXLITE_SECRET:k>")

			resp, err := client.Do(req)
			if err != nil {
				return fmt.Errorf("request %d: failed: %w", i, err)
			}
			defer resp.Body.Close()

			body, err := io.ReadAll(resp.Body)
			if err != nil {
				return fmt.Errorf("request %d: read failed: %w", i, err)
			}

			// Response leg: reflected value scrubbed back to the placeholder.
			got := string(body)
			if strings.Contains(got, "real-value") {
				return fmt.Errorf("request %d: secret leaked back to guest: %s", i, got)
			}
			return nil
		})
	}

	if err := g.Wait(); err != nil {
		t.Fatal(err)
	}

	// Request leg: all 10 streams reached upstream with the substituted value.
	auths := captured.snapshotAuths()
	if len(auths) != 10 {
		t.Errorf("expected 10 requests at upstream, got %d", len(auths))
	}
	for i, a := range auths {
		if !strings.Contains(a, "real-value") || strings.Contains(a, "BOXLITE_SECRET") {
			t.Errorf("stream %d: upstream did not receive substituted value: %s", i, a)
		}
	}
}

// --- Streaming Tests ---

func TestMitmProxy_ChunkedRequestBody(t *testing.T) {
	ca := newTestCA(t)

	secrets := testSecrets()

	var captured capturedUpstream
	addr, cleanup := startTestUpstream(t, func(w http.ResponseWriter, r *http.Request) {
		b, _ := io.ReadAll(r.Body)
		captured.addBody(string(b))
		w.Write(b)
	})
	defer cleanup()

	client := dialThroughMITM(t, ca, "api.example.com", addr, secrets)

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	// Chunked body with placeholder split across chunks won't happen here,
	// but the placeholder is embedded in a chunk.
	body := strings.NewReader("chunk1-<BOXLITE_SECRET:k>-chunk2")
	req, err := http.NewRequestWithContext(ctx, "POST", "https://api.example.com/data", body)
	if err != nil {
		t.Fatal(err)
	}
	// Don't set Content-Length to force chunked encoding
	req.ContentLength = -1

	resp, err := client.Do(req)
	if err != nil {
		t.Fatal("request failed:", err)
	}
	defer resp.Body.Close()

	got, err := io.ReadAll(resp.Body)
	if err != nil {
		t.Fatal("failed to read response:", err)
	}

	// Request leg: upstream received the substituted chunked body.
	bodies := captured.snapshotBodies()
	if len(bodies) != 1 || bodies[0] != "chunk1-real-value-chunk2" {
		t.Errorf("expected upstream to receive substituted body, got: %v", bodies)
	}
	// Response leg: reflected value scrubbed back to the placeholder.
	if string(got) != "chunk1-<BOXLITE_SECRET:k>-chunk2" {
		t.Errorf("expected scrubbed response, got %q", string(got))
	}
}

func TestMitmProxy_StreamingRequestBody(t *testing.T) {
	ca := newTestCA(t)

	secrets := testSecrets()

	received := make(chan string, 1)
	addr, cleanup := startTestUpstream(t, func(w http.ResponseWriter, r *http.Request) {
		b, _ := io.ReadAll(r.Body)
		received <- string(b)
		w.WriteHeader(http.StatusOK)
	})
	defer cleanup()

	client := dialThroughMITM(t, ca, "api.example.com", addr, secrets)

	pr, pw := io.Pipe()

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	req, err := http.NewRequestWithContext(ctx, "POST", "https://api.example.com/stream", pr)
	if err != nil {
		t.Fatal(err)
	}
	req.ContentLength = -1 // streaming

	// Write data in background
	go func() {
		pw.Write([]byte("prefix-"))
		time.Sleep(50 * time.Millisecond)
		pw.Write([]byte("<BOXLITE_SECRET:k>"))
		time.Sleep(50 * time.Millisecond)
		pw.Write([]byte("-suffix"))
		pw.Close()
	}()

	resp, err := client.Do(req)
	if err != nil {
		t.Fatal("request failed:", err)
	}
	defer resp.Body.Close()

	select {
	case got := <-received:
		expected := "prefix-real-value-suffix"
		if got != expected {
			t.Errorf("expected %q, got %q", expected, got)
		}
	case <-ctx.Done():
		t.Fatal("timeout waiting for upstream to receive body")
	}
}

func TestMitmProxy_LargeResponseStreaming(t *testing.T) {
	ca := newTestCA(t)

	secrets := testSecrets()
	const responseSize = 10 * 1024 * 1024 // 10MB

	addr, cleanup := startTestUpstream(t, func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/octet-stream")
		// Write 10MB in 64KB chunks
		chunk := make([]byte, 64*1024)
		for i := range chunk {
			chunk[i] = byte(i % 256)
		}
		written := 0
		for written < responseSize {
			n := responseSize - written
			if n > len(chunk) {
				n = len(chunk)
			}
			w.Write(chunk[:n])
			written += n
		}
	})
	defer cleanup()

	client := dialThroughMITM(t, ca, "api.example.com", addr, secrets)

	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()

	req, err := http.NewRequestWithContext(ctx, "GET", "https://api.example.com/large", nil)
	if err != nil {
		t.Fatal(err)
	}

	resp, err := client.Do(req)
	if err != nil {
		t.Fatal("request failed:", err)
	}
	defer resp.Body.Close()

	// Read all without storing in memory (stream through)
	n, err := io.Copy(io.Discard, resp.Body)
	if err != nil {
		t.Fatal("failed to read large response:", err)
	}

	if n != int64(responseSize) {
		t.Errorf("expected %d bytes, got %d", responseSize, n)
	}
}

// --- Content-Length Tests ---

func TestMitmProxy_ContentLengthAdjustment(t *testing.T) {
	ca := newTestCA(t)

	secrets := testSecrets()

	var captured capturedUpstream
	addr, cleanup := startTestUpstream(t, func(w http.ResponseWriter, r *http.Request) {
		b, _ := io.ReadAll(r.Body)
		captured.addBody(string(b))
		// Echo the body and the Content-Length the upstream saw
		fmt.Fprintf(w, "body=%s;cl=%d", string(b), r.ContentLength)
	})
	defer cleanup()

	client := dialThroughMITM(t, ca, "api.example.com", addr, secrets)

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	// Body with placeholder - after substitution length changes
	bodyStr := `{"token":"<BOXLITE_SECRET:k>"}`
	req, err := http.NewRequestWithContext(ctx, "POST", "https://api.example.com/data", strings.NewReader(bodyStr))
	if err != nil {
		t.Fatal(err)
	}
	req.Header.Set("Content-Type", "application/json")

	resp, err := client.Do(req)
	if err != nil {
		t.Fatal("request failed:", err)
	}
	defer resp.Body.Close()

	got, _ := io.ReadAll(resp.Body)
	gotStr := string(got)

	// Request leg: upstream should have received the substituted body completely.
	bodies := captured.snapshotBodies()
	if len(bodies) != 1 || bodies[0] != `{"token":"real-value"}` {
		t.Errorf("expected upstream to receive substituted body, got: %v", bodies)
	}
	// Response leg: reflected value scrubbed back to the placeholder.
	if strings.Contains(gotStr, "real-value") {
		t.Errorf("secret value leaked back to guest in response: %s", gotStr)
	}
	if !strings.Contains(gotStr, `body={"token":"<BOXLITE_SECRET:k>"}`) {
		t.Errorf("expected scrubbed response body, got: %s", gotStr)
	}
}

// TestMitmProxy_ResponseScrubbing verifies defense-in-depth: an upstream that
// reflects the secret value in a response header AND body must have both
// scrubbed back to the placeholder before the guest sees them.
func TestMitmProxy_ResponseScrubbing(t *testing.T) {
	ca := newTestCA(t)
	secrets := testSecrets()

	var captured capturedUpstream
	addr, cleanup := startTestUpstream(t, func(w http.ResponseWriter, r *http.Request) {
		auth := r.Header.Get("Authorization")
		captured.addAuth(auth)
		// Hostile/echoing upstream reflects the received (already-substituted)
		// value back in both a response header and the body.
		w.Header().Set("X-Echo-Auth", auth)
		fmt.Fprintf(w, "echo:%s", auth)
	})
	defer cleanup()

	client := dialThroughMITM(t, ca, "api.example.com", addr, secrets)

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	req, err := http.NewRequestWithContext(ctx, "GET", "https://api.example.com/test", nil)
	if err != nil {
		t.Fatal(err)
	}
	req.Header.Set("Authorization", "Bearer <BOXLITE_SECRET:k>")

	resp, err := client.Do(req)
	if err != nil {
		t.Fatal("request failed:", err)
	}
	defer resp.Body.Close()
	body, _ := io.ReadAll(resp.Body)

	// Sanity: request substitution reached upstream.
	auths := captured.snapshotAuths()
	if len(auths) != 1 || !strings.Contains(auths[0], "real-value") {
		t.Fatalf("expected upstream to receive substituted value, got: %v", auths)
	}

	// Response header must be scrubbed.
	if h := resp.Header.Get("X-Echo-Auth"); strings.Contains(h, "real-value") {
		t.Errorf("secret leaked in response header: %s", h)
	} else if !strings.Contains(h, "<BOXLITE_SECRET:k>") {
		t.Errorf("expected placeholder in scrubbed response header, got: %s", h)
	}

	// Response body must be scrubbed.
	if strings.Contains(string(body), "real-value") {
		t.Errorf("secret leaked in response body: %s", string(body))
	}
	if !strings.Contains(string(body), "<BOXLITE_SECRET:k>") {
		t.Errorf("expected placeholder in scrubbed response body, got: %s", string(body))
	}
}

// TestMitmProxy_ResponseScrubbing_Gzip verifies the scrubber is not bypassed by
// response compression: an upstream that reflects the secret inside a
// gzip-compressed body must still have it scrubbed before the guest sees it.
func TestMitmProxy_ResponseScrubbing_Gzip(t *testing.T) {
	ca := newTestCA(t)
	secrets := testSecrets()

	var captured capturedUpstream
	addr, cleanup := startTestUpstream(t, func(w http.ResponseWriter, r *http.Request) {
		auth := r.Header.Get("Authorization")
		captured.addAuth(auth)
		// Reflect the (substituted) value inside a gzip body, regardless of
		// Accept-Encoding (like httpbin's /gzip endpoint).
		w.Header().Set("Content-Encoding", "gzip")
		gz := gzip.NewWriter(w)
		fmt.Fprintf(gz, `{"auth":%q}`, auth)
		gz.Close()
	})
	defer cleanup()

	client := dialThroughMITM(t, ca, "api.example.com", addr, secrets)

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	req, err := http.NewRequestWithContext(ctx, "GET", "https://api.example.com/test", nil)
	if err != nil {
		t.Fatal(err)
	}
	req.Header.Set("Authorization", "Bearer <BOXLITE_SECRET:k>")
	req.Header.Set("Accept-Encoding", "gzip") // attacker explicitly asks for compression

	resp, err := client.Do(req)
	if err != nil {
		t.Fatal("request failed:", err)
	}
	defer resp.Body.Close()
	body, _ := io.ReadAll(resp.Body)

	// Sanity: upstream received the substituted value.
	auths := captured.snapshotAuths()
	if len(auths) != 1 || !strings.Contains(auths[0], "real-value") {
		t.Fatalf("expected upstream to receive substituted value, got: %v", auths)
	}
	// The compressed reflection must be decompressed, scrubbed, and served as
	// identity — the guest must not see the plaintext value.
	if strings.Contains(string(body), "real-value") {
		t.Errorf("secret leaked through gzip response: %s", string(body))
	}
	if !strings.Contains(string(body), "<BOXLITE_SECRET:k>") {
		t.Errorf("expected placeholder in scrubbed response, got: %s", string(body))
	}
	if enc := resp.Header.Get("Content-Encoding"); enc != "" {
		t.Errorf("expected Content-Encoding stripped after decompress, got %q", enc)
	}
}

// --- Error Handling Tests ---

func TestMitmProxy_UpstreamError(t *testing.T) {
	ca := newTestCA(t)

	secrets := testSecrets()

	// Use a port that is definitely closed
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	closedAddr := ln.Addr().String()
	ln.Close() // Close immediately so nothing is listening

	client := dialThroughMITM(t, ca, "api.example.com", closedAddr, secrets)

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	req, err := http.NewRequestWithContext(ctx, "GET", "https://api.example.com/test", nil)
	if err != nil {
		t.Fatal(err)
	}

	resp, err := client.Do(req)
	if err != nil {
		// Connection error is acceptable
		return
	}
	defer resp.Body.Close()

	// If we got a response, it should be an error status (502)
	if resp.StatusCode != http.StatusBadGateway {
		t.Errorf("expected 502 or connection error, got status %d", resp.StatusCode)
	}
}

func TestMitmProxy_UpstreamSlowResponse(t *testing.T) {
	ca := newTestCA(t)

	secrets := testSecrets()

	addr, cleanup := startTestUpstream(t, func(w http.ResponseWriter, r *http.Request) {
		time.Sleep(500 * time.Millisecond) // Simulate slow upstream
		fmt.Fprint(w, "slow-response")
	})
	defer cleanup()

	client := dialThroughMITM(t, ca, "api.example.com", addr, secrets)
	client.Timeout = 5 * time.Second // Longer than the upstream delay

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	req, err := http.NewRequestWithContext(ctx, "GET", "https://api.example.com/slow", nil)
	if err != nil {
		t.Fatal(err)
	}

	resp, err := client.Do(req)
	if err != nil {
		t.Fatal("request should have succeeded despite slow upstream:", err)
	}
	defer resp.Body.Close()

	body, _ := io.ReadAll(resp.Body)
	if string(body) != "slow-response" {
		t.Errorf("expected 'slow-response', got %q", string(body))
	}
}

func TestMitmProxy_GuestDisconnect(t *testing.T) {
	ca := newTestCA(t)

	secrets := testSecrets()

	addr, cleanup := startTestUpstream(t, func(w http.ResponseWriter, r *http.Request) {
		time.Sleep(5 * time.Second) // Very slow — guest will disconnect first
		fmt.Fprint(w, "too-late")
	})
	defer cleanup()

	goroutinesBefore := runtime.NumGoroutine()

	guestConn, proxyConn := net.Pipe()

	go mitmAndForward(proxyConn, "api.example.com", addr, ca, secrets, &tls.Config{InsecureSkipVerify: true})

	// Close guest side immediately to simulate disconnect
	guestConn.Close()

	// Wait for goroutines to settle
	time.Sleep(500 * time.Millisecond)

	goroutinesAfter := runtime.NumGoroutine()
	// Allow some tolerance (±5 goroutines for runtime overhead)
	if goroutinesAfter > goroutinesBefore+5 {
		t.Errorf("possible goroutine leak: before=%d, after=%d", goroutinesBefore, goroutinesAfter)
	}
}

func TestMitmProxy_EmptyBody(t *testing.T) {
	ca := newTestCA(t)

	secrets := testSecrets()

	addr, cleanup := startTestUpstream(t, func(w http.ResponseWriter, r *http.Request) {
		fmt.Fprint(w, "ok")
	})
	defer cleanup()

	client := dialThroughMITM(t, ca, "api.example.com", addr, secrets)

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	req, err := http.NewRequestWithContext(ctx, "GET", "https://api.example.com/empty", nil)
	if err != nil {
		t.Fatal(err)
	}

	resp, err := client.Do(req)
	if err != nil {
		t.Fatal("GET with no body should not panic:", err)
	}
	defer resp.Body.Close()

	body, _ := io.ReadAll(resp.Body)
	if string(body) != "ok" {
		t.Errorf("expected 'ok', got %q", string(body))
	}
}

// --- Routing Tests ---

func TestMitmProxy_NoSecretHost_Passthrough(t *testing.T) {
	// Verify that SecretHostMatcher correctly identifies non-secret hosts
	secrets := testSecrets()
	matcher := NewSecretHostMatcher(secrets)

	// api.example.com is in secrets' Hosts list
	if !matcher.Matches("api.example.com") {
		// Note: this will fail with stub since Matches always returns false.
		// That's expected — will pass once implemented.
		t.Error("expected api.example.com to match as a secret host")
	}

	// random.example.com is NOT in any secret's Hosts list
	if matcher.Matches("random.example.com") {
		t.Error("expected random.example.com to NOT match as a secret host")
	}
}

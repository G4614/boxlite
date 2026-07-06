package main

import (
	"compress/flate"
	"compress/gzip"
	"context"
	"crypto/tls"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/http/httputil"
	"strings"
	"time"

	logrus "github.com/sirupsen/logrus"
	"golang.org/x/net/http2"
)

const upstreamDialTimeout = 30 * time.Second

// mitmAndForward handles a MITM'd connection: TLS termination, reverse proxy, secret substitution.
// upstreamTLSConfig overrides the TLS config for upstream connections (nil = system defaults).
func mitmAndForward(guestConn net.Conn, hostname string, destAddr string, ca *BoxCA, secrets []SecretConfig, upstreamTLSConfig ...*tls.Config) {
	cert, err := ca.GenerateHostCert(hostname)
	if err != nil {
		logrus.WithError(err).WithField("hostname", hostname).Error("MITM: cert generation failed")
		guestConn.Close()
		return
	}

	tlsGuest := tls.Server(guestConn, &tls.Config{
		GetCertificate: func(*tls.ClientHelloInfo) (*tls.Certificate, error) {
			return cert, nil
		},
		NextProtos: []string{"h2", "http/1.1"},
	})

	upstreamTransport := &http.Transport{
		ForceAttemptHTTP2: true,
		TLSClientConfig:   resolveUpstreamTLS(hostname, upstreamTLSConfig...),
		DialContext: func(ctx context.Context, network, _ string) (net.Conn, error) {
			return (&net.Dialer{Timeout: upstreamDialTimeout}).DialContext(ctx, network, destAddr)
		},
	}

	proxy := &httputil.ReverseProxy{
		Director: func(req *http.Request) {
			req.URL.Scheme = "https"
			req.URL.Host = hostname
			req.Host = hostname // HTTP/1.1 Host header must match
			// Headers substituted here; body substituted in secretTransport.RoundTrip
			substituteHeaders(req, secrets)
		},
		Transport: &secretTransport{
			inner:   upstreamTransport,
			secrets: secrets,
		},
		FlushInterval: -1,
		ErrorHandler: func(w http.ResponseWriter, r *http.Request, err error) {
			logrus.WithFields(logrus.Fields{
				"hostname": hostname,
				"path":     r.URL.Path,
				"error":    err,
			}).Warn("MITM: upstream error")
			w.WriteHeader(http.StatusBadGateway)
		},
	}

	if err := tlsGuest.Handshake(); err != nil {
		logrus.WithError(err).WithField("hostname", hostname).Debug("MITM: TLS handshake failed")
		guestConn.Close()
		return
	}

	if tlsGuest.ConnectionState().NegotiatedProtocol == "h2" {
		h2srv := &http2.Server{}
		h2srv.ServeConn(tlsGuest, &http2.ServeConnOpts{Handler: proxy})
	} else {
		// HTTP/1.1: use http.Server with a proper shutdown mechanism.
		// After the single connection closes, shut down the server to avoid
		// leaking a goroutine blocked in Accept().
		listener := newSingleConnListener(tlsGuest)
		srv := &http.Server{Handler: proxy}
		srv.Serve(listener) //nolint:errcheck
		// Serve returns when the connection closes — shut down to release resources
		srv.Close()
	}
}

// singleConnListener serves exactly one pre-accepted connection as a net.Listener.
type singleConnListener struct {
	ch     chan net.Conn
	addr   net.Addr
	closed chan struct{}
}

func newSingleConnListener(conn net.Conn) *singleConnListener {
	l := &singleConnListener{
		ch:     make(chan net.Conn, 1),
		addr:   conn.LocalAddr(),
		closed: make(chan struct{}),
	}
	l.ch <- conn
	return l
}

func (l *singleConnListener) Accept() (net.Conn, error) {
	select {
	case conn := <-l.ch:
		return conn, nil
	case <-l.closed:
		return nil, net.ErrClosed
	}
}

func (l *singleConnListener) Close() error {
	select {
	case <-l.closed:
	default:
		close(l.closed)
	}
	return nil
}

func (l *singleConnListener) Addr() net.Addr { return l.addr }

// Threat model for secret substitution:
//
// What it protects: the real secret never lives in the guest (config/env/image/
// snapshot) — the guest only ever holds the placeholder. Substitution happens
// host-side, on egress, and only for connections whose hostname matches the
// secret's Hosts allow-list (see SecretsForHost). The upstream's real
// certificate is verified (resolveUpstreamTLS sets ServerName), so the secret
// is never handed to a TLS impostor.
//
// What it does NOT guarantee: confidentiality of the plaintext against the
// guest itself. Request substitution is one-directional, so an allow-listed
// upstream that reflects the secret (echo endpoints, verbose errors) would
// return the plaintext to the guest. secretTransport scrubs response headers
// and bodies (value -> placeholder) as defense-in-depth against such accidental
// reflection, but a MALICIOUS guest that can transform the value before it is
// echoed can still recover it. Keep Hosts tightly scoped to trusted upstreams.

// secretTransport wraps http.RoundTripper to inject streaming body replacement
// on the request and scrub reflected secrets out of the response.
type secretTransport struct {
	inner   http.RoundTripper
	secrets []SecretConfig
}

func (t *secretTransport) RoundTrip(req *http.Request) (*http.Response, error) {
	if len(t.secrets) > 0 {
		// The response scrubber scans bytes for reflected secret values. Opaque
		// compression (gzip/deflate/br/...) would hide the plaintext from that
		// scan, so ask the upstream not to compress. Non-compliant upstreams are
		// handled in installResponseBodyScrubber.
		req.Header.Set("Accept-Encoding", "identity")
		if req.Body != nil {
			req.Body = newStreamingReplacer(req.Body, t.secrets)
			req.ContentLength = -1
			req.Header.Del("Content-Length")
		}
	}
	resp, err := t.inner.RoundTrip(req)
	if err != nil || resp == nil || len(t.secrets) == 0 {
		return resp, err
	}
	// Defense-in-depth: scrub any reflected secret values back to placeholders
	// so a reflecting upstream cannot hand the plaintext to the guest.
	scrubResponseHeaders(resp, t.secrets)
	if resp.Body != nil {
		if err := installResponseBodyScrubber(resp, t.secrets); err != nil {
			resp.Body.Close()
			logrus.WithError(err).Warn("MITM: refusing unscrubbable response")
			return nil, err
		}
	}
	return resp, err
}

// installResponseBodyScrubber wraps resp.Body so reflected secret values are
// scrubbed even when the upstream compressed the response. A byte-level scrubber
// cannot see plaintext inside gzip/deflate, so we decompress, scrub, and serve
// identity. Encodings we cannot decode are refused (fail closed) rather than
// passed through unscrubbed.
func installResponseBodyScrubber(resp *http.Response, secrets []SecretConfig) error {
	enc := strings.ToLower(strings.TrimSpace(resp.Header.Get("Content-Encoding")))
	switch enc {
	case "", "identity":
		resp.Body = newSecretResponseScrubber(resp.Body, secrets)
		resp.ContentLength = -1
		resp.Header.Del("Content-Length")
		return nil
	case "gzip":
		zr, err := gzip.NewReader(resp.Body)
		if err != nil {
			return fmt.Errorf("gzip reader: %w", err)
		}
		installDecodedScrubber(resp, secrets, zr)
		return nil
	case "deflate":
		installDecodedScrubber(resp, secrets, flate.NewReader(resp.Body))
		return nil
	default:
		// We asked for identity but the upstream used an encoding we cannot
		// decode, so we cannot verify the body is free of the secret.
		return fmt.Errorf("cannot scrub response with Content-Encoding %q", enc)
	}
}

// installDecodedScrubber replaces resp.Body with a reader that decompresses via
// decoder, scrubs the plaintext, and is served as identity. It closes both the
// decoder and the original compressed body.
func installDecodedScrubber(resp *http.Response, secrets []SecretConfig, decoder io.ReadCloser) {
	src := &multiCloseReader{Reader: decoder, closers: []io.Closer{decoder, resp.Body}}
	resp.Body = newSecretResponseScrubber(src, secrets)
	resp.Header.Del("Content-Encoding")
	resp.Header.Del("Content-Length")
	resp.ContentLength = -1
}

// multiCloseReader adapts a Reader plus a set of Closers into an io.ReadCloser.
type multiCloseReader struct {
	io.Reader
	closers []io.Closer
}

func (m *multiCloseReader) Close() error {
	var firstErr error
	for _, c := range m.closers {
		if err := c.Close(); err != nil && firstErr == nil {
			firstErr = err
		}
	}
	return firstErr
}

// scrubResponseHeaders replaces leaked secret values with their placeholders in
// response headers and trailers (defense-in-depth against header reflection).
func scrubResponseHeaders(resp *http.Response, secrets []SecretConfig) {
	if resp == nil || len(secrets) == 0 {
		return
	}
	pairs := make([]string, 0, len(secrets)*2)
	for _, s := range secrets {
		if s.Value == "" {
			continue
		}
		pairs = append(pairs, s.Value, s.Placeholder)
	}
	if len(pairs) == 0 {
		return
	}
	r := strings.NewReplacer(pairs...)
	scrub := func(h http.Header) {
		for key, vals := range h {
			for i, v := range vals {
				h[key][i] = r.Replace(v)
			}
		}
	}
	scrub(resp.Header)
	scrub(resp.Trailer)
}

package main

import (
	"context"
	"crypto/tls"
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
	if req.Body != nil && len(t.secrets) > 0 {
		req.Body = newStreamingReplacer(req.Body, t.secrets)
		req.ContentLength = -1
		req.Header.Del("Content-Length")
	}
	resp, err := t.inner.RoundTrip(req)
	if err != nil || resp == nil || len(t.secrets) == 0 {
		return resp, err
	}
	// Defense-in-depth: scrub any reflected secret values back to placeholders
	// so a reflecting upstream cannot hand the plaintext to the guest.
	scrubResponseHeaders(resp, t.secrets)
	if resp.Body != nil {
		resp.Body = newSecretResponseScrubber(resp.Body, t.secrets)
		// Body length changes when value/placeholder differ in size.
		resp.ContentLength = -1
		resp.Header.Del("Content-Length")
	}
	return resp, err
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

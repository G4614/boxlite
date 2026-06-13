package main

import (
	"crypto/ed25519"
	"crypto/rand"
	"encoding/base64"
	"net"
	"testing"

	"golang.org/x/crypto/ssh"
)

func newTestHostKey(t *testing.T) (ssh.PublicKey, string) {
	t.Helper()
	pub, priv, err := ed25519.GenerateKey(rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	signer, err := ssh.NewSignerFromKey(priv)
	if err != nil {
		t.Fatal(err)
	}
	_ = pub
	authorized := ssh.MarshalAuthorizedKey(signer.PublicKey()) // "ssh-ed25519 AAAA...\n"
	return signer.PublicKey(), base64.StdEncoding.EncodeToString(authorized)
}

// TestBuildRunnerHostKeyCallback_PinsConfiguredKey verifies that a configured
// RUNNER_HOST_KEY pins the runner's host key: the matching key is accepted and a
// different key is rejected. Before the fix the dial used
// ssh.InsecureIgnoreHostKey(), which accepts ANY key (MITM possible).
func TestBuildRunnerHostKeyCallback_PinsConfiguredKey(t *testing.T) {
	wantPub, b64 := newTestHostKey(t)
	otherPub, _ := newTestHostKey(t)

	cb, verified, err := buildRunnerHostKeyCallback(b64)
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if !verified {
		t.Fatal("expected verification enabled for a configured key")
	}

	addr := &net.TCPAddr{IP: net.IPv4(127, 0, 0, 1), Port: 2220}
	if err := cb("runner:2220", addr, wantPub); err != nil {
		t.Errorf("pinned callback rejected the matching host key: %v", err)
	}
	if err := cb("runner:2220", addr, otherPub); err == nil {
		t.Error("pinned callback accepted a DIFFERENT host key — MITM not prevented")
	}
}

// TestBuildRunnerHostKeyCallback_EmptyPreservesBehavior documents that an unset
// RUNNER_HOST_KEY keeps the previous (unverified) behavior rather than breaking
// existing deployments — but reports verification as disabled.
func TestBuildRunnerHostKeyCallback_EmptyPreservesBehavior(t *testing.T) {
	cb, verified, err := buildRunnerHostKeyCallback("")
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if verified {
		t.Fatal("expected verification disabled when no key configured")
	}
	anyPub, _ := newTestHostKey(t)
	if err := cb("runner:2220", &net.TCPAddr{}, anyPub); err != nil {
		t.Errorf("unconfigured callback should accept any key (legacy behavior), got: %v", err)
	}
}

func TestBuildRunnerHostKeyCallback_InvalidKey(t *testing.T) {
	if _, _, err := buildRunnerHostKeyCallback("!!!not-base64!!!"); err == nil {
		t.Error("expected error for invalid base64 RUNNER_HOST_KEY")
	}
}

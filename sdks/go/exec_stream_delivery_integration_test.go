//go:build boxlite_dev

package boxlite

import (
	"context"
	"sync"
	"testing"
	"time"
)

type blockingSink struct {
	entered     chan struct{}
	release     chan struct{}
	enteredOnce sync.Once
}

func newBlockingSink() *blockingSink {
	return &blockingSink{
		entered: make(chan struct{}),
		release: make(chan struct{}),
	}
}

func (s *blockingSink) Write(p []byte) (int, error) {
	s.enteredOnce.Do(func() { close(s.entered) })
	<-s.release
	return len(p), nil
}

// TestIntegrationStdoutBlockingSinkStallsRuntimeDrain reproduces the real
// deadlock fixed here: one execution's stdout writer blocks forever. Stream
// delivery must not run inline on the shared Runtime drain goroutine, otherwise
// a second, unrelated exec on the same Runtime never receives its completion.
func TestIntegrationStdoutBlockingSinkStallsRuntimeDrain(t *testing.T) {
	rt := newTestRuntime(t)
	box := createStartedBoxOrSkip(t, rt, "alpine:latest", WithAutoRemove(false))

	sink := newBlockingSink()
	execA, err := box.StartExecution(context.Background(), "sh", []string{"-c", "echo drain-wedge; sleep 60"}, &ExecutionOptions{
		Stdout: sink,
	})
	if err != nil {
		t.Fatalf("StartExecution(A): %v", err)
	}
	t.Cleanup(func() {
		close(sink.release)
		_ = execA.Close()
	})

	select {
	case <-sink.entered:
	case <-time.After(20 * time.Second):
		t.Skip("execution A never produced stdout within 20s; runtime/guest unavailable")
	}

	ctxB, cancel := context.WithTimeout(context.Background(), 8*time.Second)
	defer cancel()

	res, err := box.Exec(ctxB, "echo", "probe")
	if err != nil {
		t.Fatalf("second exec on the same Runtime did not complete while execution A's stdout sink was blocked: %v", err)
	}
	if res.Stdout != "probe\n" {
		t.Fatalf("probe exec stdout = %q, want %q", res.Stdout, "probe\n")
	}
}

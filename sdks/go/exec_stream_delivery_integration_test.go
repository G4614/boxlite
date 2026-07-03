//go:build boxlite_dev

package boxlite

import (
	"context"
	"sync"
	"testing"
	"time"
)

// blockingSink is an io.Writer that parks the goroutine calling Write until
// the test releases it. The first Write closes `entered` so the test can
// observe — without a wall-clock sleep — that delivery has reached the sink,
// then blocks on `release` (never sent during the test body) so the caller
// stays parked.
//
// The point of parking here is to prove a misbehaving user sink cannot wedge
// the shared per-Runtime drain goroutine. With per-execution delivery
// (exec.go), Write runs on execution A's OWN delivery goroutine, so parking it
// stalls only A — the drain keeps dispatching events for every other
// execution on the same Runtime.
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

// TestIntegrationStdoutBlockingSinkStallsRuntimeDrain guards against
// head-of-line blocking on the shared drain goroutine. Execution A streams one
// stdout chunk into a sink that blocks forever; execution B is a trivial
// command on the SAME Runtime and must complete promptly.
//
// If stream delivery ran inline on the single per-Runtime drain goroutine (the
// pre-fix behaviour), A's blocked sink would wedge that goroutine and B's Wait
// completion — queued behind A's stdout in the one event FIFO — would never be
// dispatched, so B's context deadline would fire. With delivery moved onto a
// per-execution chain, A's blocked sink stalls only A's own delivery goroutine
// and B completes. This asserts that fixed behaviour: a regression that put
// stream delivery back on the drain thread would time out here.
func TestIntegrationStdoutBlockingSinkStallsRuntimeDrain(t *testing.T) {
	rt := newTestRuntime(t)
	box := createStartedBoxOrSkip(t, rt, "alpine:latest", WithAutoRemove(false))

	sink := newBlockingSink()

	// Execution A: emit one stdout chunk, then stay alive. The chunk is
	// delivered into `sink`, parking A's delivery goroutine. We never Wait on A.
	execA, err := box.StartExecution(context.Background(), "sh", []string{"-c", "echo drain-wedge; sleep 60"}, &ExecutionOptions{
		Stdout: sink,
	})
	if err != nil {
		t.Fatalf("StartExecution(A): %v", err)
	}

	// Tear down in the order that lets the runtime shut down cleanly: release
	// the parked sink FIRST (close release), explicitly kill execution A (it is
	// still in `sleep 60`, so don't rely on Close to terminate it), then free
	// it. Registered LAST so LIFO runs it BEFORE the box/runtime cleanups from
	// the helpers — otherwise rt.Close could block waiting on the parked
	// delivery.
	t.Cleanup(func() {
		close(sink.release)
		ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		defer cancel()
		_ = execA.Kill(ctx)
		_ = execA.Close()
	})

	// Wait (on a channel, not a sleep) until delivery has provably reached the
	// blocking sink before probing.
	select {
	case <-sink.entered:
	case <-time.After(20 * time.Second):
		t.Skip("execution A never produced stdout within 20s; runtime/guest unavailable, cannot exercise the drain-blocking path")
	}

	// Execution B: a trivial command on the SAME runtime. It must complete even
	// while A's sink is parked.
	ctxB, cancel := context.WithTimeout(context.Background(), 8*time.Second)
	defer cancel()

	start := time.Now()
	res, err := box.Exec(ctxB, "echo", "probe")
	elapsed := time.Since(start)

	if err != nil {
		t.Fatalf("head-of-line blocking: a second exec on the same Runtime did not "+
			"complete in %s while execution A's stdout sink was blocked — stream "+
			"delivery must not run on the shared drain goroutine: %v", elapsed, err)
	}
	if res.Stdout != "probe\n" {
		t.Fatalf("probe exec returned unexpected stdout %q (want %q)", res.Stdout, "probe\n")
	}
}

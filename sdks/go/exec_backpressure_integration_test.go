//go:build boxlite_dev

package boxlite

import (
	"context"
	"runtime"
	"sync"
	"testing"
	"time"
)

type blockForeverSink struct {
	once    sync.Once
	entered chan struct{}
	rel     chan struct{}
}

func (b *blockForeverSink) Write(p []byte) (int, error) {
	b.once.Do(func() {
		close(b.entered)
		<-b.rel
	})
	return len(p), nil
}

// TestIntegrationExecBackpressureBoundsMemory proves that the per-exec stream
// queue added to isolate the shared Runtime drain cannot grow without bound
// when a caller's sink is blocked and the guest keeps producing output.
func TestIntegrationExecBackpressureBoundsMemory(t *testing.T) {
	rt := newTestRuntime(t)
	box := createStartedBoxOrSkip(t, rt, "alpine:latest", WithAutoRemove(false))

	sink := &blockForeverSink{
		entered: make(chan struct{}),
		rel:     make(chan struct{}),
	}
	trigger := "/tmp/boxlite-backpressure-go"
	exec, err := box.StartExecution(context.Background(), "sh",
		[]string{"-c", "while [ ! -f " + trigger + " ]; do sleep 0.1; done; dd if=/dev/zero bs=1M count=128 2>/dev/null"}, &ExecutionOptions{Stdout: sink})
	if err != nil {
		t.Fatalf("StartExecution: %v", err)
	}

	var cleanupOnce sync.Once
	cleanup := func() {
		cleanupOnce.Do(func() {
			ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
			defer cancel()
			// Gentle teardown: unblock the sink so buffered output drains and the
			// stream pumps reach EOF, kill the process, and Wait so the pumps
			// finish (and the exit callback fires) BEFORE Close. Closing an
			// execution whose stdout is still actively flooding is a separate
			// concern — the C force-close abort race — and is out of scope for
			// this memory-bound test.
			close(sink.rel)
			_ = exec.Kill(ctx)
			_, _ = exec.Wait(ctx)
			_ = exec.Close()
		})
	}
	t.Cleanup(cleanup)

	touchCtx, touchCancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer touchCancel()
	if _, err := box.Exec(touchCtx, "sh", "-c", "touch "+trigger); err != nil {
		t.Fatalf("trigger output: %v", err)
	}

	select {
	case <-sink.entered:
	case <-time.After(20 * time.Second):
		t.Skip("execution never delivered stdout to the blocking sink within 20s")
	}

	var m runtime.MemStats
	read := func() uint64 {
		runtime.ReadMemStats(&m)
		return m.HeapAlloc
	}

	base := read()
	for i := 0; i < 6; i++ {
		time.Sleep(500 * time.Millisecond)
	}
	grew := int64(read()) - int64(base)

	cleanup()

	const bound = 64 << 20
	if grew > bound {
		t.Fatalf("heap grew %d bytes over 3s under a blocked sink; want plateau under %d", grew, bound)
	}
}

package boxlite

import (
	"sync"
	"sync/atomic"
	"testing"
	"time"
)

// gatedSink blocks every Write until released, then records the bytes so the
// test can assert byte-exact, in-order delivery once backpressure lifts.
type gatedSink struct {
	release chan struct{}
	mu      sync.Mutex
	got     []byte
}

func (g *gatedSink) Write(p []byte) (int, error) {
	<-g.release
	g.mu.Lock()
	g.got = append(g.got, p...)
	g.mu.Unlock()
	return len(p), nil
}

// TestExecutionStreamStateBackpressureBoundsBuffer covers the redesigned
// per-execution chain (PR #826, revised): there is no Go->C pause call. When a
// sink is blocked, deliverStdout — which runs on the shared drain goroutine —
// must block once buffered bytes reach the high-water mark instead of growing
// the chain without bound. That block is exactly what stalls the shared C
// event queue so push_event yields and the guest is throttled. Once the sink
// drains, every byte must still be delivered in order.
//
// This is the deterministic, VM-free counterpart to the integration tests
// TestIntegrationExecBackpressureBoundsMemory and
// TestIntegrationStdoutBlockingSinkStallsRuntimeDrain.
func TestExecutionStreamStateBackpressureBoundsBuffer(t *testing.T) {
	sink := &gatedSink{release: make(chan struct{})}
	state := newExecutionStreamState(ExecutionOptions{Stdout: sink})

	const chunkSize = 64 << 10 // 64 KiB
	// Four times the high-water mark's worth of chunks, so the producer is
	// guaranteed to wedge on backpressure long before it finishes.
	const chunks = (streamQueueHighWater / chunkSize) * 4

	mkChunk := func(i int) []byte {
		b := make([]byte, chunkSize)
		for j := range b {
			b[j] = byte(i) // distinct per chunk (chunks == 256, no wrap)
		}
		return b
	}

	var accepted int64
	done := make(chan struct{})
	// Buffered to `chunks` so the producer never blocks on this channel — the
	// only thing that may block it is the backpressure under test.
	progress := make(chan int64, chunks)
	go func() {
		defer close(done)
		for i := 0; i < chunks; i++ {
			state.deliverStdout(mkChunk(i))
			progress <- atomic.AddInt64(&accepted, 1)
		}
	}()

	// Drive the wait off the producer's own progress signals (no sleep
	// polling): once buffered bytes reach the high-water mark the next enqueue
	// blocks, so `accepted` climbs to the mark and then stalls there.
	target := int64(streamQueueHighWater / chunkSize)
	timeout := time.After(5 * time.Second)
	for atomic.LoadInt64(&accepted) < target {
		select {
		case <-progress:
		case <-timeout:
			t.Fatalf("producer only accepted %d chunks before timeout; want >= %d "+
				"(backpressure never let the chain fill to the high-water mark)",
				atomic.LoadInt64(&accepted), target)
		}
	}

	if got := atomic.LoadInt64(&accepted); got >= chunks {
		t.Fatalf("producer accepted all %d chunks with the sink blocked; "+
			"backpressure never engaged (chain grew unbounded)", chunks)
	}

	// Buffered bytes must sit within one chunk of the high-water mark: the chain
	// filled up to the mark (lower bound → backpressure engaged at the right
	// point) and no further (upper bound → memory is bounded). deliverLoop is
	// wedged on the blocked sink, so nothing has been drained yet.
	state.qmu.Lock()
	queued := state.queuedBytes
	state.qmu.Unlock()
	if queued < streamQueueHighWater-chunkSize || queued > streamQueueHighWater+chunkSize {
		t.Fatalf("buffered %d bytes, want within one chunk of high-water %d "+
			"(backpressure should plateau the chain at the mark)", queued, streamQueueHighWater)
	}

	// Release the sink; the blocked producer and deliverLoop must drain fully.
	close(sink.release)
	select {
	case <-done:
	case <-time.After(10 * time.Second):
		t.Fatal("producer did not finish after the sink was released")
	}

	// Exit ends the chain; the drain barrier guarantees every chunk is flushed.
	state.deliverExit(0)
	select {
	case <-state.drained:
	case <-time.After(10 * time.Second):
		t.Fatal("stream chain did not drain after exit")
	}

	sink.mu.Lock()
	defer sink.mu.Unlock()
	if len(sink.got) != chunks*chunkSize {
		t.Fatalf("delivered %d bytes, want %d", len(sink.got), chunks*chunkSize)
	}
	for i := 0; i < chunks; i++ {
		off := i * chunkSize
		if want := byte(i); sink.got[off] != want || sink.got[off+chunkSize-1] != want {
			t.Fatalf("chunk %d corrupted or out of order: first=%d last=%d, want %d",
				i, sink.got[off], sink.got[off+chunkSize-1], want)
		}
	}
}

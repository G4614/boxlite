//go:build unix

package boxlite

/*
#include <stdlib.h>
#include "bridge.h"
*/
import "C"
import (
	"context"
	"fmt"
	"net"
	"os"
	goruntime "runtime"
	"unsafe"
)

// NetworkHandle exposes network operations for a box.
type NetworkHandle struct {
	handle *C.CBoxNetworkHandle
}

func closedNetworkError() error {
	return &Error{Code: ErrInvalidState, Message: "network handle is closed"}
}

// Network returns a handle for box network operations.
func (b *Box) Network() (*NetworkHandle, error) {
	if b == nil || b.handle == nil {
		return nil, ErrRuntimeClosed
	}
	var cNetwork *C.CBoxNetworkHandle
	var cerr C.CBoxliteError
	code := C.boxlite_box_network(b.handle, &cNetwork, &cerr)
	if code != C.Ok {
		return nil, freeError(&cerr)
	}
	if cNetwork == nil {
		return nil, fmt.Errorf("boxlite network handle is nil")
	}
	network := &NetworkHandle{handle: cNetwork}
	goruntime.SetFinalizer(network, func(network *NetworkHandle) { network.Close() })
	return network, nil
}

// Close releases the network handle. It does not close the box.
func (n *NetworkHandle) Close() {
	if n == nil || n.handle == nil {
		return
	}
	C.boxlite_box_network_free(n.handle)
	n.handle = nil
	goruntime.SetFinalizer(n, nil)
}

// Tunnel opens a raw byte stream to target inside the box's guest network.
func (n *NetworkHandle) Tunnel(ctx context.Context, target string) (net.Conn, error) {
	host, portText, err := net.SplitHostPort(target)
	if err != nil {
		return nil, err
	}
	port, err := net.LookupPort("tcp", portText)
	if err != nil {
		return nil, err
	}
	if port < 0 || port > 65535 {
		return nil, fmt.Errorf("invalid tunnel port %d", port)
	}
	return n.openTunnel(ctx, host, uint16(port))
}

// Tunnel opens a raw byte stream to target inside the box's guest network.
func (b *Box) Tunnel(ctx context.Context, target string) (net.Conn, error) {
	network, err := b.Network()
	if err != nil {
		return nil, err
	}
	defer network.Close()
	return network.Tunnel(ctx, target)
}

func (n *NetworkHandle) openTunnel(ctx context.Context, targetIP string, targetPort uint16) (net.Conn, error) {
	select {
	case <-ctx.Done():
		return nil, ctx.Err()
	default:
	}
	if n == nil || n.handle == nil {
		return nil, closedNetworkError()
	}

	var cFD C.int
	var cerr C.CBoxliteError
	if targetIP == "" {
		return nil, fmt.Errorf("tunnel target IP is required")
	}
	cIP := C.CString(targetIP)
	defer C.free(unsafe.Pointer(cIP))
	code := C.boxlite_box_network_tunnel(n.handle, cIP, C.uint16_t(targetPort), &cFD, &cerr)
	if code != C.Ok {
		return nil, freeError(&cerr)
	}
	if cFD < 0 {
		return nil, fmt.Errorf("boxlite tunnel returned invalid fd")
	}
	file := os.NewFile(uintptr(cFD), "boxlite-tunnel")
	if file == nil {
		return nil, fmt.Errorf("boxlite tunnel returned invalid fd")
	}
	defer file.Close()
	conn, err := net.FileConn(file)
	if err != nil {
		return nil, err
	}

	select {
	case <-ctx.Done():
		_ = conn.Close()
		return nil, ctx.Err()
	default:
		return conn, nil
	}
}

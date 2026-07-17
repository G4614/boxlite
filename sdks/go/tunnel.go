//go:build unix

package boxlite

/*
#include "bridge.h"
*/
import "C"
import (
	"context"
	"fmt"
	"net"
	"os"
)

// EndpointType identifies how clients can reach a box service tunnel.
type EndpointType int

const (
	EndpointTypeURL EndpointType = iota
	EndpointTypeUnixSocket
)

// Endpoint describes a stable box service tunnel endpoint.
type Endpoint struct {
	Type    EndpointType
	Address string
}

// Network is a box-scoped handle for network operations.
type Network struct {
	handle *C.CBoxNetworkHandle
}

// Tunnel is a reusable connection target for a service inside a box.
type Tunnel struct {
	handle *C.CBoxTunnelHandle
}

// Network returns the box-scoped handle for network operations.
func (b *Box) Network() (*Network, error) {
	if b == nil || b.handle == nil {
		return nil, ErrRuntimeClosed
	}

	var cNetwork *C.CBoxNetworkHandle
	var cerr C.CBoxliteError
	code := C.boxlite_box_network(b.handle, &cNetwork, &cerr)
	if code != C.Ok {
		return nil, freeError(&cerr)
	}

	return &Network{handle: cNetwork}, nil
}

// Close releases the network handle.
func (n *Network) Close() error {
	if n != nil && n.handle != nil {
		C.boxlite_box_network_free(n.handle)
		n.handle = nil
	}
	return nil
}

// Tunnel prepares a reusable endpoint for a service port inside the box.
func (n *Network) Tunnel(ctx context.Context, port uint16) (*Tunnel, error) {
	if n == nil || n.handle == nil {
		return nil, ErrRuntimeClosed
	}
	if port == 0 {
		return nil, fmt.Errorf("invalid tunnel port %d", port)
	}
	if err := ctx.Err(); err != nil {
		return nil, err
	}

	var cTunnel *C.CBoxTunnelHandle
	var cerr C.CBoxliteError
	code := C.boxlite_box_network_tunnel(n.handle, C.uint16_t(port), &cTunnel, &cerr)
	if code != C.Ok {
		return nil, freeError(&cerr)
	}
	return &Tunnel{handle: cTunnel}, nil
}

// Close releases the tunnel handle.
func (t *Tunnel) Close() error {
	if t != nil && t.handle != nil {
		C.boxlite_box_tunnel_free(t.handle)
		t.handle = nil
	}
	return nil
}

// Endpoint returns the stable URL or Unix socket prepared for the tunnel.
func (t *Tunnel) Endpoint() (Endpoint, error) {
	if t == nil || t.handle == nil {
		return Endpoint{}, ErrRuntimeClosed
	}

	var endpointType uint32
	var address *C.char
	var cerr C.CBoxliteError
	code := C.boxlite_box_tunnel_endpoint(t.handle, &endpointType, &address, &cerr)
	if code != C.Ok {
		return Endpoint{}, freeError(&cerr)
	}
	defer C.boxlite_free_string(address)

	var typ EndpointType
	switch endpointType {
	case C.BoxliteEndpointTypeUrl:
		typ = EndpointTypeURL
	case C.BoxliteEndpointTypeUnixSocket:
		typ = EndpointTypeUnixSocket
	default:
		return Endpoint{}, fmt.Errorf("boxlite returned unknown endpoint type %d", endpointType)
	}
	return Endpoint{Type: typ, Address: C.GoString(address)}, nil
}

// Connect opens a new raw byte stream through the tunnel.
func (t *Tunnel) Connect(ctx context.Context) (net.Conn, error) {
	if t == nil || t.handle == nil {
		return nil, ErrRuntimeClosed
	}
	if err := ctx.Err(); err != nil {
		return nil, err
	}

	var cerr C.CBoxliteError
	var cFD C.int
	code := C.boxlite_box_tunnel_connect(t.handle, &cFD, &cerr)
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
	if err := ctx.Err(); err != nil {
		_ = conn.Close()
		return nil, err
	}
	return conn, nil
}

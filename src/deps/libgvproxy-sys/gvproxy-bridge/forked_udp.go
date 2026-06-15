package main

// forked_udp.go — Optional AllowNet filtering for UDP.
//
// allow_net only constrained TCP: UDP went through gvproxy's default NAT
// forwarder, so a restricted box could exfiltrate to any host over UDP/QUIC
// (audit finding #2). This installs a UDP forwarder that drops datagrams to
// destinations not permitted by the AllowNet filter.
//
// WIP / NOT INTEGRATION-VERIFIED: the policy decision (udpAllowed) is unit
// tested, but the live forwarder path — in particular whether overriding the UDP
// protocol handler bypasses gvproxy's embedded DNS resolver endpoint, and the
// datagram relay/idle-timeout behavior — has NOT been validated against a running
// network stack. It is therefore gated behind BOXLITE_UDP_FILTER=true and is OFF
// by default (live behavior unchanged). See OverrideUDPHandlerIfEnabled.

import (
	"fmt"
	"net"
	"reflect"
	"sync"
	"time"
	"unsafe"

	"github.com/containers/gvisor-tap-vsock/pkg/types"
	"github.com/containers/gvisor-tap-vsock/pkg/virtualnetwork"
	logrus "github.com/sirupsen/logrus"
	"gvisor.dev/gvisor/pkg/tcpip"
	"gvisor.dev/gvisor/pkg/tcpip/adapters/gonet"
	"gvisor.dev/gvisor/pkg/tcpip/stack"
	"gvisor.dev/gvisor/pkg/tcpip/transport/udp"
	"gvisor.dev/gvisor/pkg/waiter"
)

// udpIdleTimeout closes an idle forwarded UDP flow so endpoints don't leak.
const udpIdleTimeout = 60 * time.Second

// udpAllowed reports whether a UDP datagram to (destIP, destPort) is permitted.
//
// With no AllowNet filter configured (filter == nil) all UDP is allowed, exactly
// as before. With a filter, only destinations whose IP is allowed by the filter
// pass — this includes the gateway/internal IPs (always-allow), so DNS to the
// gateway resolver still works, while UDP to a non-allowlisted host (including
// DNS-over-UDP exfiltration to an attacker on port 53, or QUIC/HTTP-3 on 443) is
// dropped.
func udpAllowed(destIP net.IP, destPort uint16, filter *TCPFilter) bool {
	_ = destPort // reserved: port-specific policy may be added later
	if filter == nil {
		return true
	}
	return filter.MatchesIP(destIP)
}

// OverrideUDPHandlerIfEnabled installs the filtered UDP forwarder only when a
// filter is configured AND BOXLITE_UDP_FILTER=true. Default (env unset) is a
// no-op so the live UDP path is unchanged until the forwarder is integration
// verified.
func OverrideUDPHandlerIfEnabled(
	vn *virtualnetwork.VirtualNetwork,
	config *types.Configuration,
	filter *TCPFilter,
	enabled bool,
) error {
	if filter == nil || !enabled {
		return nil
	}
	return OverrideUDPHandler(vn, config, filter)
}

// OverrideUDPHandler replaces the default UDP protocol handler with one that
// enforces the AllowNet filter. Mirrors OverrideTCPHandler's reflective access
// to the VirtualNetwork's private stack field.
func OverrideUDPHandler(
	vn *virtualnetwork.VirtualNetwork,
	config *types.Configuration,
	filter *TCPFilter,
) error {
	v := reflect.ValueOf(vn).Elem()
	stackField := v.FieldByName("stack")
	if !stackField.IsValid() {
		return fmt.Errorf("VirtualNetwork has no 'stack' field (gvisor-tap-vsock API changed?)")
	}
	// #nosec G103 — accessing private field to override UDP handler, same as TCP.
	s := (*stack.Stack)(unsafe.Pointer(stackField.Pointer()))

	nat := make(map[tcpip.Address]tcpip.Address)
	for source, destination := range config.NAT {
		nat[tcpip.AddrFrom4Slice(net.ParseIP(source).To4())] =
			tcpip.AddrFrom4Slice(net.ParseIP(destination).To4())
	}
	var natLock sync.Mutex

	fwd := udp.NewForwarder(s, func(r *udp.ForwarderRequest) {
		localAddress := r.ID().LocalAddress
		destIP, dialAddress := resolveTCPDestination(localAddress, nat, &natLock)
		destPort := r.ID().LocalPort

		if !udpAllowed(destIP, destPort, filter) {
			logrus.WithFields(logrus.Fields{
				"dst_ip":   destIP,
				"dst_port": destPort,
			}).Info("allowNet UDP: blocked (no matching rule)")
			return // drop: do not create an endpoint
		}

		var wq waiter.Queue
		ep, udpErr := r.CreateEndpoint(&wq)
		if udpErr != nil {
			logrus.Debugf("udp r.CreateEndpoint() = %v", udpErr)
			return
		}
		guestConn := gonet.NewUDPConn(&wq, ep)

		destAddr := fmt.Sprintf("%s:%d", dialAddress, destPort)
		outbound, err := net.Dial("udp", destAddr)
		if err != nil {
			logrus.Tracef("udp net.Dial(%s) = %v", destAddr, err)
			guestConn.Close()
			return
		}

		go relayUDP(guestConn, outbound)
		go relayUDP(outbound, guestConn)
	})

	s.SetTransportProtocolHandler(udp.ProtocolNumber, fwd.HandlePacket)
	logrus.Info("allowNet UDP: handler overridden with filtering forwarder (experimental)")
	return nil
}

// relayUDP copies datagrams one way until an idle timeout or error, then closes
// both ends so the paired copier unblocks.
func relayUDP(dst, src net.Conn) {
	buf := make([]byte, 65535)
	for {
		_ = src.SetReadDeadline(time.Now().Add(udpIdleTimeout))
		n, err := src.Read(buf)
		if n > 0 {
			if _, werr := dst.Write(buf[:n]); werr != nil {
				break
			}
		}
		if err != nil {
			break
		}
	}
	src.Close()
	dst.Close()
}

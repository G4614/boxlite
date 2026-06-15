package main

import (
	"sync"
	"testing"
	"time"

	"gvisor.dev/gvisor/pkg/buffer"
	"gvisor.dev/gvisor/pkg/tcpip"
	"gvisor.dev/gvisor/pkg/tcpip/checksum"
	"gvisor.dev/gvisor/pkg/tcpip/header"
	"gvisor.dev/gvisor/pkg/tcpip/link/channel"
	"gvisor.dev/gvisor/pkg/tcpip/network/ipv4"
	"gvisor.dev/gvisor/pkg/tcpip/stack"
	"gvisor.dev/gvisor/pkg/tcpip/transport/udp"
	"gvisor.dev/gvisor/pkg/waiter"

	"gvisor.dev/gvisor/pkg/tcpip/adapters/gonet"
)

// injectUDP crafts an IPv4+UDP datagram and injects it inbound on the NIC,
// exactly as a guest packet would arrive at the stack.
func injectUDP(ch *channel.Endpoint, src, dst tcpip.Address, dstPort uint16, payload []byte) {
	udpSize := header.UDPMinimumSize + len(payload)
	total := header.IPv4MinimumSize + udpSize
	buf := make([]byte, total)

	ip := header.IPv4(buf)
	ip.Encode(&header.IPv4Fields{
		TotalLength: uint16(total),
		TTL:         64,
		Protocol:    uint8(udp.ProtocolNumber),
		SrcAddr:     src,
		DstAddr:     dst,
	})
	ip.SetChecksum(^ip.CalculateChecksum())

	u := header.UDP(buf[header.IPv4MinimumSize:])
	u.Encode(&header.UDPFields{SrcPort: 40000, DstPort: dstPort, Length: uint16(udpSize)})
	copy(buf[header.IPv4MinimumSize+header.UDPMinimumSize:], payload)
	xsum := header.PseudoHeaderChecksum(udp.ProtocolNumber, src, dst, uint16(udpSize))
	xsum = checksum.Checksum(payload, xsum)
	u.SetChecksum(^u.CalculateChecksum(xsum))

	pkt := stack.NewPacketBuffer(stack.PacketBufferOptions{Payload: buffer.MakeWithData(buf)})
	ch.InjectInbound(ipv4.ProtocolNumber, pkt)
	pkt.DecRef()
}

// TestUDPFilter_BoundDNSEndpointNotBypassed verifies the load-bearing assumption
// behind OverrideUDPHandler: gvproxy binds its DNS resolver as a UDP *endpoint*
// (services.go: gonet.DialUDP gateway:53), and gvisor's transport demuxer
// delivers to a bound endpoint BEFORE the SetTransportProtocolHandler forwarder.
// So replacing the UDP handler to enforce allow_net does NOT bypass DNS: the
// resolver endpoint still receives :53 traffic, while the filtering forwarder
// only sees guest→internet UDP (which it drops when not allow-listed).
func TestUDPFilter_BoundDNSEndpointNotBypassed(t *testing.T) {
	s := stack.New(stack.Options{
		NetworkProtocols:   []stack.NetworkProtocolFactory{ipv4.NewProtocol},
		TransportProtocols: []stack.TransportProtocolFactory{udp.NewProtocol},
	})
	defer s.Close()

	ch := channel.New(16, 1500, "")
	if err := s.CreateNIC(1, ch); err != nil {
		t.Fatalf("CreateNIC: %v", err)
	}
	gateway := tcpip.AddrFrom4([4]byte{192, 168, 127, 1})
	if err := s.AddProtocolAddress(1, tcpip.ProtocolAddress{
		Protocol:          ipv4.ProtocolNumber,
		AddressWithPrefix: gateway.WithPrefix(),
	}, stack.AddressProperties{}); err != nil {
		t.Fatalf("AddProtocolAddress: %v", err)
	}
	s.SetSpoofing(1, true)
	s.SetPromiscuousMode(1, true)
	s.SetRouteTable([]tcpip.Route{{Destination: header.IPv4EmptySubnet, NIC: 1}})

	// Bind a UDP endpoint on gateway:53 — stands in for gvproxy's DNS resolver.
	dnsConn, err := gonet.DialUDP(s, &tcpip.FullAddress{NIC: 1, Addr: gateway, Port: 53}, nil, ipv4.ProtocolNumber)
	if err != nil {
		t.Fatalf("DialUDP(gateway:53): %v", err)
	}
	defer dnsConn.Close()

	// Install a filtering forwarder (the production mechanism) that records which
	// dest ports reach it and drops everything (filter denies).
	var mu sync.Mutex
	forwarded := map[uint16]bool{}
	fwd := udp.NewForwarder(s, func(r *udp.ForwarderRequest) {
		mu.Lock()
		forwarded[r.ID().LocalPort] = true
		mu.Unlock()
		// deny: create then immediately close so nothing is relayed
		var wq waiter.Queue
		if ep, e := r.CreateEndpoint(&wq); e == nil {
			ep.Close()
		}
	})
	s.SetTransportProtocolHandler(udp.ProtocolNumber, fwd.HandlePacket)

	src := tcpip.AddrFrom4([4]byte{192, 168, 127, 2})

	// (1) DNS to gateway:53 — must reach the bound resolver endpoint, NOT the forwarder.
	injectUDP(ch, src, gateway, 53, []byte("dns-query"))
	gotDNS := make([]byte, 64)
	_ = dnsConn.SetReadDeadline(time.Now().Add(2 * time.Second))
	n, _, rerr := dnsConn.ReadFrom(gotDNS)
	if rerr != nil || string(gotDNS[:n]) != "dns-query" {
		t.Fatalf("DNS resolver endpoint did not receive :53 datagram (err=%v, got=%q) — override BYPASSED DNS", rerr, string(gotDNS[:n]))
	}
	mu.Lock()
	dnsHitForwarder := forwarded[53]
	mu.Unlock()
	if dnsHitForwarder {
		t.Error(":53 reached the filtering forwarder — DNS would be filtered/broken")
	}

	// (2) Guest→internet UDP to a port with no bound endpoint — must reach the
	// forwarder (where allow_net filtering / drop happens).
	injectUDP(ch, src, gateway, 9999, []byte("exfil"))
	deadline := time.Now().Add(2 * time.Second)
	for {
		mu.Lock()
		hit := forwarded[9999]
		mu.Unlock()
		if hit {
			break
		}
		if time.Now().After(deadline) {
			t.Fatal("non-DNS UDP never reached the filtering forwarder")
		}
		time.Sleep(10 * time.Millisecond)
	}
}

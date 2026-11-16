# ADR-0003: QUIC (quinn) as the transport protocol

- Status: Accepted
- Date: 2026-11-16

## Context
We carry three kinds of traffic with different needs: control messages (reliable, ordered), pen samples (lowest latency, loss-tolerant) and video (high bandwidth, stale data worthless). We also need encryption and authentication on untrusted networks.

Alternatives: raw TCP + UDP sockets with our own crypto (more code, more risk); WebRTC (excellent for NAT traversal and media, but heavy, complex to embed, and we don't need NAT traversal); WebSocket (TCP head-of-line blocking hurts pen latency).

## Decision
Use QUIC via the `quinn` crate on a single UDP port:
- control → one reliable bidirectional stream;
- pen → QUIC datagrams (unreliable) with sequence numbers and batched historical samples, plus state transitions mirrored on the control stream;
- video → unidirectional streams, reset when superseded by a newer keyframe interval.
TLS 1.3 with pinned self-signed certificates established at pairing.

## Consequences
- + Encryption, congestion control, multiplexing and datagrams from one mature library.
- + No head-of-line blocking between pen and video.
- − UDP may trigger a firewall prompt on first run (Windows); must be explained in onboarding.
- − Some networks block UDP; USB tethering is the fallback.

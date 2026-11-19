# ADR-0009: TLS over TCP as the fallback transport

- Status: Accepted
- Date: 2026-11-19

## Context
QUIC (ADR-0003) needs UDP. Some networks block or throttle it, and some links carry only TCP:
`adb reverse` forwards TCP ports over a USB cable, which is the only cable link a Mac has to a
tablet that tethers over RNDIS (macOS has no RNDIS driver). The plan always listed a TCP/TLS
fallback; this records its shape.

One TCP connection with our own multiplexing would be simplest, but TCP delivers bytes in order:
a pen sample written after a video frame waits until the whole frame is through, and a lost packet
stalls everything behind it. That is the head-of-line blocking QUIC avoids.

## Decision
- A TCP session is **three TLS 1.3 connections** (control, video, pen) on the QUIC port number,
  grouped by a random token in a short preamble and by the client certificate. The tablet opens all
  three; certificates, pinning and pairing are unchanged. Details in `docs/protocol.md`.
- The pen connection carries pen batches and a ping each way every second: it gives the round trip
  and ends a silent session after 5 s, as QUIC's idle timeout does. A full pen queue drops new
  samples, as a lost datagram would.
- The video connection's send buffer is 128 KB, like QUIC's send window, so a stalled link blocks
  the writer and the pipeline skips frames instead of sending old ones late.
- The tablet tries QUIC first and adds TCP after a 500 ms head start (or at once if QUIC fails);
  the first to connect is used. It tries `127.0.0.1` over TCP before anything else, for `adb reverse`.
- `adb reverse` itself is an Advanced option of the host (`--adb`), because it needs Android's
  platform tools and USB debugging.

## Consequences
- + Works where UDP doesn't; gives macOS (and Windows/Linux without tethering) a USB cable path.
- + Pen and video stay independent, as with QUIC.
- − Lost packets still stall each TCP stream (retransmission), so on lossy Wi-Fi QUIC remains
  preferred; the head start keeps it first.
- − Three sockets per session; the host caps pending handshakes (32) and drops incomplete groups
  after 5 s.
- − Link statistics over TCP are limited to the round trip.

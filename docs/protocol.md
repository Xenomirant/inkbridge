# Inkbridge protocol (v0.1)

Source of truth: the `ib-protocol` crate. This document explains it; if they disagree, the code wins and this file gets fixed.

## Connection
One QUIC connection (TLS 1.3, certificates pinned at pairing) per session, on a single UDP port. A TCP+TLS fallback carries the same messages over length-prefixed streams when UDP is blocked; pen datagrams then travel on a dedicated stream.

| Channel | QUIC primitive | Payload | Reliability |
|---|---|---|---|
| Control | bidirectional stream, opened by the client | `ControlMessage`, each prefixed with a varint length | reliable, ordered |
| Pen | datagrams (client → host) | one `PenBatch` per datagram (≤ 24 samples, ≤ 1200 bytes) | unreliable; `seq` drops duplicates and reordering |
| Video | unidirectional streams (host → client), one per keyframe interval | `VideoChunk`s, varint-length-prefixed | reliable within a stream; stale streams are reset |

Encoding: [postcard](https://postcard.jamesmunns.com/) (compact, varint-based). Each message must decode with no trailing bytes and be at most `MAX_MESSAGE_LEN` (8 MiB).

## Handshake
1. Client → `ClientHello { version, device_name, caps, screen_width, screen_height }`.
2. Host checks `version.major`, and that the client's certificate is paired:
   - OK → `HostHello { version, host_name, caps, codec, region, transport }`;
   - otherwise → `Reject { reason, host_version }` and the connection closes.
3. Both sides use the intersection of their `PenFeatures`; `codec` is the first entry in the host's preference list that the client also supports (`None` = pen-only session).

## Running session
- **Pen:** the client sends `PenBatch` datagrams as soon as samples arrive. Each `PenSample` has coordinates normalised to the streamed region (`0..=1`), pressure, tilt, rotation, state (`Hover`/`Contact`/`OutOfRange`), tool and buttons. Every change of state, tool or buttons is **also** sent as `ControlMessage::PenTransition` on the reliable stream, so a lost datagram can never leave the pen "stuck down". The host sanitises every sample before injecting it.
- **Clock sync:** either side sends `Ping { t0 }` about once a second, and the peer answers `Pong { t0, t1, t2 }`. `ib-clock::ClockSync` keeps a min-RTT estimate of the offset.
- **Video:** `VideoChunk { frame_no, kind, capture_t_us, encode_done_t_us, payload }` with an Annex-B bitstream. The client may send `RequestKeyframe` (decoder reset, loss) and `BitrateHint`.
- **Region:** either side may send `SetRegion` to change the mapped desktop area.
- **Displays (0.2):** a host with more than one display sends `Displays` (names, bounds, the one
  shown) after the hello and after every change. The client may send `SelectDisplay` with an index
  into that list; the host answers with `Displays` and then `SetRegion`, and the video restarts with
  a keyframe. Hosts send `Displays` only to clients whose hello says 0.2 or newer.
- **End:** `Goodbye`, then close. Hosts release the pen (`PenSink::release`) on any disconnect.

## Versioning
`PROTOCOL_VERSION` (currently 0.1). A bump of `minor` adds optional fields or messages that old peers can ignore through capability negotiation; a bump of `major` is breaking. Until 1.0, any minor bump may also be breaking.

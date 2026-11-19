# Inkbridge protocol (v0.1)

Source of truth: the `ib-protocol` crate. This document explains it; if they disagree, the code wins and this file gets fixed.

## Connection
One QUIC connection (TLS 1.3, certificates pinned at pairing) per session, on a single UDP port. A TCP+TLS fallback carries the same messages when UDP is blocked or only TCP is carried (`adb reverse`): see below and ADR-0009.

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
  a keyframe. Hosts send `Displays` only to clients whose hello says 0.2 or newer. A host that
  can make the tablet an extra screen ends the list with its offers (entries without a size);
  choosing one adds a display, which then appears in the list as the current one.
- **End:** `Goodbye`, then close. Hosts release the pen (`PenSink::release`) on any disconnect.

## Versioning
`PROTOCOL_VERSION` (currently 0.1). A bump of `minor` adds optional fields or messages that old peers can ignore through capability negotiation; a bump of `major` is breaking. Until 1.0, any minor bump may also be breaking.

## TCP fallback (ADR-0009)
Same port number as QUIC, TLS 1.3 with the same certificates, pinning and ALPN. A session is **three
TCP connections**, all opened by the tablet, so pen samples never queue behind video inside one TCP
stream:

| Preamble channel byte | Carries |
|---|---|
| `0` control | the control stream, framed as over QUIC |
| `1` video | host → tablet `VideoChunk`s, framed as over QUIC; the host's socket send buffer is 128 KB |
| `2` pen | frames `[kind u8][len u32 BE][body]`: `0` = `PenBatch`, `1` = ping (8-byte sender time), `2` = pong (echo) |

Each connection starts, after the TLS handshake, with a 17-byte preamble: the channel byte and a
16-byte random session token chosen by the tablet. The host groups the three connections by
(client certificate fingerprint, token); a group that isn't complete within 5 s is dropped. Both
sides ping on the pen connection every second; the round trip comes from the pongs, and 5 s of
silence ends the session (QUIC's idle timeout). Unknown pen frame kinds are skipped.

The tablet tries `127.0.0.1` over TCP first (a computer reached through `adb reverse`; nothing
listening there fails at once), then each address from the QR code with QUIC, adding TCP if QUIC
hasn't connected within 500 ms. The host reports a tablet on its loopback as `TransportKind::Usb`.

# ADR-0006: Latency architecture — local ink, separate pen path, measured stages

- Status: Accepted
- Date: 2026-11-16

## Context
Handwriting feels broken above roughly 50 ms of pen-to-ink delay. A network round trip plus capture, encode and decode cannot get close to the ~10–20 ms of a native pen display on its own.

## Decision
1. **Local ink first.** The client draws the stroke immediately on a front-buffered overlay (Android `graphics-core` + `MotionEventPredictor`; iPad predicted touches) and hands over to the real desktop pixels when a frame containing that ink arrives. Perceived latency becomes the tablet's own.
2. **The pen path is independent of video.** Samples are sent unbuffered as QUIC datagrams (batched historical samples), with contact/button transitions mirrored reliably. On the host they're injected on a dedicated high-priority thread, never through an async executor queue.
3. **The video path never queues.** Capture on change only, one frame in flight, encoder without frame delay, decoder presents immediately (`KEY_LOW_LATENCY` where available).
4. **Every stage is measured.** Packets carry timestamps; `ib-clock` synchronises clocks (min-RTT NTP-style) and keeps per-stage percentiles (`Stage::*`). These are shown in a debug overlay, and `tools/latency-probe` measures end to end. Regressions in these numbers block PRs.
5. **Adaptive quality protects latency.** Bitrate and resolution drop before latency rises.

## Consequences
- + Writing feels immediate even over Wi-Fi.
- + Latency regressions are visible and attributable to a stage.
- − The local overlay and desktop pixels can briefly disagree (e.g. the app renders a different brush); the overlay fades out quickly to keep that invisible.
- − Clock sync and timestamps add a few bytes per packet.

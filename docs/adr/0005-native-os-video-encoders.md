# ADR-0005: Native OS video encoders, zero-copy, latency over quality

- Status: Accepted
- Date: 2026-11-16

## Context
Latency is the main product metric. Options: FFmpeg (via `rsmpeg`; what Sunshine and RustDesk use), GStreamer, or native OS APIs. The hardware encoder chips are the same in every case (NVENC/QSV/AMF/VideoToolbox/VA-API); the differences are copies, buffering, control over low-latency settings, and maintenance.

## Decision
- Native backends behind `VideoEncoder`: **Media Foundation** (Windows, D3D11 textures), **VideoToolbox** (macOS/iOS), **VA-API** (Linux); **openh264** as the software fallback.
- **H.264** by default (hardware decode everywhere, including older tablets); HEVC optional when both sides support it.
- Mandatory latency settings: no B-frames, zero lookahead, `MaxFrameDelay = 0` (no frame queue), intra-refresh instead of periodic large keyframes, CBR/capped VBR. The `encode` call returns the output for the frame it was given.
- A direct **NVENC** backend is added only if `ib-clock` stage measurements show Media Foundation adds latency on NVIDIA GPUs.

## Consequences
- + No 30–80 MB bundled library and no FFmpeg API churn.
- + Direct control of every low-latency knob; GPU texture → encoder with no CPU copy.
- + OS drivers already handle older GPUs; openh264 covers machines without hardware encode.
- − Three backends to write and maintain instead of one.
- − Some vendor-specific tuning (e.g. NVENC's ultra-low-latency preset) may need the direct backend.

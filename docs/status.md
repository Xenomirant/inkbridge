# Status (updated 2026-11-17)

Where the project stands against the milestones in `docs/plan.md`. Update this file when a milestone
moves or an open issue is found or fixed.

## Done
- **M0 Foundations:** workspace, protocol, geometry, clock, backend traits with fakes, CI, licences, ADRs 0001–0007.
- **M1 Pen over Wi-Fi (Android → Windows):** QUIC transport with QR pairing and pinned identities;
  Windows pen injection (synthetic pointer device: pressure, tilt, eraser, barrel button); Android app
  with QR pairing and full-rate stylus capture. Verified in a browser pen test page (`tools/pen-test`).
- **M2 Screen streaming (Windows):** WGC capture, zero-copy Media Foundation H.264 (hardware), openh264
  fallback, adaptive bitrate, per-stage latency telemetry on the tablet, front-buffered local ink with
  motion prediction, touchpad-style two-finger scrolling, native touch gestures, portrait/landscape,
  zoomable view (View toggle button).
- **M3 USB (Windows):** Android USB tethering (RNDIS); the computer dials the tablet over the USB link;
  optional `--fix-usb-routing` keeps internet traffic off the tablet.

## In progress — M3 Linux host
Implemented and unit-tested (see `docs/linux.md`):
- X11 capture (x11rb) and uinput pressure pen (opt-in `--setup-uinput`).
- Wayland: portal screen sharing + PipeWire capture, portal pointer fallback (no pressure).
- VA-API H.264 encoder (own bindgen bindings, own SPS/PPS writer). Verified on the RTX 5060 via
  Mesa d3d12 in WSL only.
- `inkbridge-host --self-test` and the live-USB kit (`tools/linux-validation`).

**Not yet validated on a real Linux desktop.** Paused to do macOS first; resume by booting the
live-USB kit (Ubuntu 26.04.1; Intel iGPU encodes via iHD, the RTX runs nouveau there).

## Next — M4 macOS host
Plan (`docs/plan.md` M4): ScreenCaptureKit capture (macOS 12.3+), VideoToolbox H.264 (real-time,
no frame reordering), pen via CGEvent tablet pointer + proximity events (needs the Accessibility
permission), permission onboarding (Screen Recording, Accessibility). Avoid virtual HID (CoreHID
pressure bug FB24785043 on macOS 26). Test pressure in Krita, Photoshop, Affinity, Preview/Notes, OneNote.

Things to settle early:
- **USB on macOS:** Android USB tethering defaults to RNDIS, which macOS doesn't support. Check whether
  the tablet can tether over CDC-NCM (macOS supports it natively); otherwise Wi-Fi only, or
  `adb reverse` as an Advanced option.
- New backends go behind the existing traits: `ScreenSource` (ib-capture), `VideoEncoder` (ib-encode),
  `PenSink` (ib-inject), wired in `crates/ib-host/src/platform.rs` and `video.rs` like Windows/Linux.
- macOS frames can stay on the GPU (IOSurface → VideoToolbox), like the Windows zero-copy path.

## Open issues
- Windows: root cause of occasional rejected pen events (a resync workaround exists, commit 945d526).
- Possible brief flicker on the tablet after switching the View (zoom) mode.
- `docs/latency.md` with measured end-to-end numbers (Wi-Fi vs USB, RTX vs iGPU) is still to write.
- Linux NVIDIA: VA-API can't use NVENC; NVIDIA GPUs fall back to openh264. A direct NVENC backend is a
  follow-up (load `libnvidia-encode.so` at runtime; MIT-licensed headers).

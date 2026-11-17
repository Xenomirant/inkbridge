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

**Not yet validated on a real Linux desktop.** Paused to do macOS first (on the MacBook, cloned from
GitHub); resume by booting the
live-USB kit (Ubuntu 26.04.1; Intel iGPU encodes via iHD, the RTX runs nouveau there).

## In progress — M4 macOS host
Implemented on the MacBook (Apple silicon, macOS 27), tested with the TB331FC over Wi-Fi:
- **Pen:** CGEvent tablet-point events with proximity (`ib-inject/src/macos.rs`). Works with the real
  pen. The app matrix (Krita, Preview/Notes, others) and which apps honour pressure are still to record.
- **Capture:** ScreenCaptureKit (`ib-capture/src/macos.rs`), main display only. CPU frames (BGRA) for
  the software encoder, GPU pixel buffers (NV12) for VideoToolbox. The system scales frames to the
  encoder's size.
- **Encoder:** VideoToolbox H.264 (`ib-encode/src/videotoolbox.rs`): low-latency rate control, one
  reference frame, Main profile, AVCC converted to Annex B with SPS/PPS on every keyframe. Falls back
  to CPU capture + openh264 when no hardware encoder is available.
  Encode time for the full 3420x2214 screen at 1680x1088: about 6 ms (median) against 55 ms with
  openh264. Output verified with an independent decoder (`mac_pipeline_probe`).
  On the tablet over campus Wi-Fi (jittery, 7-180 ms round trip): about 60 ms capture-to-screen
  (median) against 258 ms with openh264, 57 fps while the screen moves. The p95 is far higher
  (0.2-2 s) and follows the Wi-Fi, not the encoder. To repeat on a quiet network for `docs/latency.md`.
- **Permissions:** asked when needed, with an explanation: Screen Recording when a tablet first
  connects, Accessibility at the first pen touch.
- **Bindings:** the `objc2` crate family (generated from Apple's headers) for all three backends.
- **Decoder limit:** the tablet reports what its hardware decoder can take (TB331FC: 1920x1088, either
  orientation) and the host keeps the picture within it.
- **App:** keeps a list of paired computers (dropped after 90 days without use). Debug builds share one
  signing key (`apps/android/app/debug.keystore`) so every machine can update the installed app.

Building the Android app on the Mac: JDK 17, Android command-line tools, NDK and `cargo-ndk`, then
`./gradlew assembleDebug` in `apps/android` with `JAVA_HOME`, `ANDROID_HOME` and `ANDROID_NDK_HOME` set.

Still to do for M4:
- `docs/latency.md` rows for macOS (software and VideoToolbox, Wi-Fi).
- **USB on macOS:** the TB331FC tethers RNDIS-only, which macOS doesn't support, and adb can't carry
  QUIC. Needs the TCP fallback first, then an opt-in adb mode. Wi-Fi only until then.
- Several displays, permission onboarding UI, notarised bundle (M6), iPad client.

## Open issues
- Windows: root cause of occasional rejected pen events (a resync workaround exists, commit 945d526).
- Possible brief flicker on the tablet after switching the View (zoom) mode.
- The host never forgets a paired tablet (`paired-devices.txt` has no expiry). The tablet side expires
  its list; the host side matters more, because a paired tablet can inject input.
- When the Wi-Fi stalls, the picture freezes until the reliable video stream catches up (the p95
  above). The pipeline now skips frames before encoding instead of sending keyframes, but a stall of
  the link itself still shows.
- The app doesn't reconnect by itself when the host restarts; the user has to tap Connect again.
- `cargo deny check` hasn't been run on the Mac (not installed there), so the licences of the `objc2`
  crates are unchecked.
- `docs/latency.md` with measured end-to-end numbers (Wi-Fi vs USB, RTX vs iGPU) is still to write.
- Linux NVIDIA: VA-API can't use NVENC; NVIDIA GPUs fall back to openh264. A direct NVENC backend is a
  follow-up (load `libnvidia-encode.so` at runtime; MIT-licensed headers).

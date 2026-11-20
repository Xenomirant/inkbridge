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
  On the tablet over campus Wi-Fi: about 34 ms capture-to-screen (median) at 45-58 fps while the
  screen moves, against 258 ms with openh264. Of that, encode with capture wait is 12 ms, network
  5 ms, the tablet's decoder 15 ms. The p95 (40-150 ms) follows the Wi-Fi.
- **Tablet decoder:** the app picks a decoder with the low-latency feature, runs it at full operating
  rate and asks Qualcomm decoders for output in decode order. On the TB331FC this cut the time a
  frame spends in the decoder from about 60 ms to 15 ms. It also holds a low-latency Wi-Fi lock while
  the pad is shown. `adb logcat -s Inkbridge:D InkbridgeVideo:D` prints the overlay figures and decode
  times.
- **Choosing a display:** from the tablet, the "Screen" button (shown when the computer has more
  than one display) lists them and switches live: the host restarts capture on the chosen display,
  moves the pen there and sends the new region. On the host, `--list-displays` numbers the displays
  and `--display N` starts on one. macOS and Windows (monitors numbered as Windows numbers them,
  `\\.\DISPLAYn`; the pen is offset by the monitor's desktop position); Linux offers the main
  display. Windows is checked with one monitor only (the PC has one).
  Regions stay relative to the shown display; the pen sink adds its origin. Protocol 0.2 adds
  `Displays` and `SelectDisplay`; the host sends the list only to apps that speak 0.2, because older
  ones stop reading at an unknown message. Checked: the host against two real displays (ignored
  test `the_tablet_switches_displays`), and on the tablet `--display 2`, the "Screen" button
  (a switch takes 120-180 ms) and the pen on the second display.
- **The tablet as an extra screen:** in the tablet's Screen menu, or `--extend` on the host. The
  host adds a virtual display (`CGVirtualDisplay`, a private API: ADR-0008), captures it and puts
  the pen on it. It follows the tablet's rotation, and stays 15 s after a session ends so a
  reconnect keeps the windows in place. Two kinds, because the TB331FC's decoders stop at 1920x1088
  while its screen is 1920x1200:
  - "fill the tablet": the display has the tablet's shape (2560x1600, 1280x800 points at 2x). The
    picture is 1740x1088, stretched by 10 % on the tablet, so text is soft.
  - "sharpest picture (thin bars)" (`--extend --sharp`): the display is exactly what the decoder
    takes (1920x1088, 960x544 points at 2x). Nothing is scaled on either side.
  Measured with a text pattern on the tablet: 9 pt text and one-pixel bars are clearly sharper
  pixel for pixel. About 32-40 ms capture-to-screen over Wi-Fi in both. Checked with the ignored
  tests `an_extra_display_appears_and_goes_away` and `the_tablet_becomes_an_extra_screen`, and on
  the tablet. Not yet checked: a pen stroke on it.
- **Keyboard from the tablet:** CGEvent key events (`ib-inject/src/macos_keys.rs`), same
  Accessibility permission as the pen. Text as characters (any language); keys and shortcuts by key
  code, a shortcut letter at its US-layout key (macOS matches Command shortcuts through a Latin
  layout, also while Cyrillic is active; on AZERTY and similar the letter lands on the US position).
  The Win key of the tablet's row is Command; alone it opens Spotlight. The row shows ⌘ ⌃ ⌥ ⇧
  (protocol 0.4 `KeyNames`). Checked with the ignored test `keys_reach_an_app`, which types into a
  helper app (`ib-inject/tests/key_receiver.swift`) through `CGEventPostToPid`, never into the
  focused app.
- **Still screen:** capture delivers nothing while the screen is still, so the pipeline encodes the
  last frame four more times (50 ms apart) to let it get sharp.
- **Full screen:** the pad draws under the system bars and under Lenovo's three-dots window bar
  (drawing area 1920x1200 instead of 1920x1141). The bar itself is the tablet's own (system setting
  `enable_temp_zuifreeformbar`); an app can't remove it.
- **App look:** one dark palette (`Ui.kt`). Home: paired computers as cards with when they were
  last used. Pad: two small glass bars (tools left, status right) that fade while the pen draws;
  the status shows computer, link and latency, and every figure after a tap. With the sharp extra
  screen the bars sit in the black strip above the picture.
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
- **USB on macOS:** the TB331FC tethers RNDIS-only, which macOS doesn't support. Now possible with
  `--adb` over the TCP fallback (see below); not yet tried on the Mac.
- Permission onboarding UI, notarised bundle (M6), iPad client.

## Done since the Mac work (Windows PC)
- **Pairings expire on the host too:** `paired-devices.txt` records each tablet's last connection; a
  tablet unused for 90 days is dropped and needs the QR code again (as the app forgets computers).
  `--list-paired` lists them, `--forget N|all` unpairs.
- **TCP fallback (ADR-0009):** where UDP is blocked or only TCP is carried, a session runs over three
  TLS/TCP connections (control, video, pen) on the same port. The tablet tries QUIC first and adds
  TCP after 500 ms. `--adb` (Advanced) keeps `adb reverse` set up for tablets on a USB cable with USB
  debugging; the app tries the loopback first, so it connects through the cable by itself, also on
  macOS. Checked by tests (QUIC black-holed, whole host sessions over TCP); on the tablet: see below.
- **Choosing a display on Windows:** as on macOS (above).
- **Checked on the tablet (Windows, adb over USB):** pairing and a session over the TCP fallback through
  `adb reverse`; the pad shows "USB".
- **Keyboard from the tablet (protocol 0.3, Windows host):** a "Keys" button on the pad opens
  Android's keyboard (any language) and a row of extra keys: Esc, Tab, Win (tap: Start; hold: keep
  it pressed for Win+key), Ctrl/Alt/Shift (stay pressed for the next key), arrows, Del, Enter. A
  hardware keyboard on the tablet works too. macOS since 2026-11-20 (below); Linux (uinput/portal)
  still says "no keyboard": to do.
- **The tablet as an extra screen on Windows (Advanced, one admin prompt):**
  `--install-virtual-display` installs the third-party Virtual Display Driver (MIT, signed by SignPath
  Foundation; pinned release and SHA-256, signature checked), rendering on the GPU of the main
  monitor, with the common tablet shapes; `--remove-virtual-display` undoes it. The driver always has
  one monitor (a count of 0 is invalid to it); the host **attaches** it to the desktop with the
  tablet's shape while the tablet is an extra screen and **detaches** it otherwise (also at start-up,
  and `--detach-virtual-display` by hand). Adding a new shape needs `RELOAD_DRIVER`, and this driver
  version's process crashes and is restarted by Windows then (Windows allows a few restarts in a
  while), so the common shapes are written at install. Note: the driver's pipe accepts commands from
  any local program. Checked: install on the PC, the ignored test
  `a_virtual_monitor_appears_and_goes_away` (no crash), and on the tablet over USB: the sharp extra
  screen (1920x1088) with the pen, 30 ms screen latency. x64 only. Windows display scaling for the
  monitor is Windows' default (change it in Settings > Display; Windows remembers it).
- **Security:** a pairing code works for 5 minutes after it is shown (and once). The host's private
  key file is owner-only on macOS/Linux (tightened on load for older installs).

## In progress — desktop app (tray / menu bar)
`apps/desktop` (Tauri 2, its own Cargo workspace; built in CI on all three OSes). Runs the same
`ib_host::service::Service` as `inkbridge-host`, in the tray (Windows, Linux) or menu bar (macOS):
- Tray icon (blue while a tablet is connected), tooltip and menu with the status, "Open Inkbridge",
  "Pair a tablet…", "Quit". Closing the window keeps it running; a second start shows the window.
- Window: status (tablet, USB/Wi-Fi, QUIC/TCP, round trip), pairing with the QR code (opens by
  itself on first start), paired tablets (most recent first, older duplicates marked) with Forget,
  "Start with this computer" (autostart plugin), and Advanced: USB through adb, the Windows extra
  screen driver (install/remove), the USB routing fix. Notifications when a tablet connects or pairs.
- Settings in `settings.json`, the log in `inkbridge.log` (both in the host's config folder).
- Icons from `apps/desktop/icons/render.py` (the Android icon's geometry, no image library).
Checked on Windows: start, window, pairing list, resizing. To do: installers (M6), macOS and Linux
runs, the tablet's display choice and keyboard state in the window, a first-run guide.

## Open issues
- Windows: root cause of occasional rejected pen events (a resync workaround exists, commit 945d526).
- Possible brief flicker on the tablet after switching the View (zoom) mode.
- Wi-Fi stalls: on the campus network the link pauses for 100-230 ms every 5-10 s and loses 14-40
  packets in a burst (host log, `RUST_LOG=info,ib_host::server=debug`, "video link" lines). The
  picture freezes for that long. QUIC used to buffer up to 10 MB of video unseen during a pause and
  deliver it late (p95 up to 720 ms); its send window is now 128 KB, so the writer blocks, the
  pipeline skips frames, and the worst p95 in a 60 s test was 227 ms. Not tried: turning AirDrop and
  Handoff off on the Mac (they make the Wi-Fi radio leave the channel), and a quiet network.
- Reconnecting: the pad reconnects by itself when the connection drops (host restarted, Wi-Fi
  blinked), and the host lets a reconnecting tablet replace its dead session at once. With the USB
  cable link up the pad first waits for the computer to dial, then tries Wi-Fi (not yet tried on a
  real USB link: the Mac has none). A stroke in progress ends at the drop; the next stroke works
  (checked by restarting the host during a 9 s stylus stroke).
- macOS: a monitor plugged in while the host runs is not offered until the host restarts (Core
  Graphics reads display modes once per process, ADR-0008).
- `cargo deny check` hasn't been run on the Mac (not installed there), so the licences of the `objc2`
  crates are unchecked.
- `docs/latency.md`: still to measure Windows over Wi-Fi, the Intel iGPU and a Linux host.
- Linux NVIDIA: VA-API can't use NVENC; NVIDIA GPUs fall back to openh264. A direct NVENC backend is a
  follow-up (load `libnvidia-encode.so` at runtime; MIT-licensed headers).

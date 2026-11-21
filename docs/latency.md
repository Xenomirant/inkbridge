# Latency

Measured end-to-end figures. "Screen" is capture to shown on the tablet (the tablet's overlay:
capture, encode, network, decode and display, using the clock offset from the session's pings).
Median with p95 in brackets. Add a row whenever something changes the path.

## Screen (computer → tablet)

| Date | Host | Encoder | Link | Picture | Screen | Encode | Network | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-11-18 | MacBook (Apple silicon, macOS 27) | VideoToolbox | Wi-Fi (campus) | 1680x1088 | 34 ms (40–150) | 12 ms incl. capture wait | 5 ms | p95 follows Wi-Fi stalls |
| 2026-11-18 | MacBook | openh264 (software) | Wi-Fi (campus) | 1680x1088 | 258 ms | 55 ms | — | why native encoders |
| 2026-11-18 | MacBook, extra screen | VideoToolbox | Wi-Fi (campus) | 1920x1088 / 1740x1088 | 32–40 ms | — | — | |
| 2026-11-20 | MacBook | VideoToolbox | USB, `adb reverse` (TCP fallback) | 1680x1088 | 33–35 ms (44–53) | 13–14 ms incl. capture wait | 2 ms | desktop app, 38–50 fps |
| 2026-11-19 | Windows 11, RTX 5060 | Media Foundation (NVENC) | USB, `adb reverse` (TCP fallback) | 1920x1088 extra screen | 30 ms (53–57) | 4 ms | 2–3 ms | round trip 3 ms |
| 2026-11-20 | Linux (live USB, GNOME Wayland), Intel iGPU | VA-API (`iHD`) | USB tethering (QUIC) | 2560x1440 → 1920x1080 | 49 ms (55–60) | 24 ms | 7 ms | CPU downscale took 17 ms of it; ~31 fps |
| 2026-11-20 | Linux, same | VA-API (`iHD`) | USB tethering (QUIC) | 2560x1440 → 1920x1080 | 37–41 ms (43–45) | 13 ms | 7 ms | threaded downscale (2.4 ms); ~31 fps, see below |
| 2026-11-20 | Linux, same | VA-API (`iHD`) | USB, `adb reverse` (TCP fallback) | 2560x1440 → 1920x1080 | 36 ms (42) | 15 ms | 4 ms | |
| 2026-11-21 | Linux, same, extra screen (GNOME virtual monitor) | VA-API (`iHD`) | USB tethering (QUIC) | 1920x1088 (no scaling) | 34 ms (59) | 9 ms | 7 ms | 43 fps while drawing: the main monitor's ~31 fps cap is its read-back |
| 2026-11-20 | Linux, same | VA-API (`iHD`) | Wi-Fi (campus, QUIC) | 2560x1440 → 1920x1080 | 36–37 ms (46–47) | 13–14 ms | 5 ms | |

Still to measure: Windows over Wi-Fi (QUIC), the Intel iGPU on Windows.

**Linux capture rate:** in the live-USB session GNOME negotiated 59.95 fps (the monitor's rate) and
drew at 60 fps (`glxgears`), but the screen cast delivered about 31 fps, with Firefox and with a
60 fps GStreamer pattern alike. The monitor hangs off the RTX 5060, which runs Mesa's NVK there (GL
through zink); the screen cast copies each frame from the GPU into memory, which seems to be the
limit. Asking PipeWire for a maximum frame rate changed nothing (GNOME offers the monitor's rate
anyway).

Re-measured on the installed system (2026-11-21, Ubuntu 26.04.1, GNOME 50.1 Wayland, NVIDIA driver
595.99.02, 60 Hz monitor on the RTX 5060), with the self-test now moving the pointer itself (the
cursor is embedded in the stream, so each move is a new frame): still a frame every ~33 ms (median
32–33 ms, p95 35 ms) with 59.95 fps negotiated, and the max-framerate request made no difference
again. So the cap isn't NVK/zink. It isn't Inkbridge either: GNOME hands over shared-memory
(`MemFd`) buffers and our process callback takes 1.7 ms median, 8.8 ms worst. GNOME delivers every
second frame; next: compare KDE Plasma (Wayland portal) and Xfce (X11 capture, no portal), then
consider DMA-BUF capture so GNOME doesn't copy frames into memory.

**Other video work on the computer** competes for the same hardware encoder. On the MacBook, two
hardware video conversions (ffmpeg, VideoToolbox) running at the same time doubled the screen
latency to about 70 ms (encode 45 ms instead of 12 ms); it returned to 34 ms as soon as they paused.
Strokes then feel worse and apps seem to misbehave (2026-11-20).

The tablet (TB331FC) spends about 15 ms of this in its hardware decoder (low-latency mode,
decode-order output), see `docs/status.md`.

## Pen (tablet → computer)

- The TB331FC samples the pen every 3.3 ms (300 Hz). Every sample reaches the computer, in order.
  About a quarter arrive in clumps of 2–8, because Android delivers stylus samples in groups.
- Sideways noise of the samples is about 0.2 px (median), under 1 px for 90 %. A smoothing filter
  (1€ filter) was simulated on recorded strokes: no visible gain, 1–9 px of lag. Not used.
- **Hardware limit at touchdown:** the pen's digitizer (`himax-stylus`) holds the position for
  17–51 ms (once 151 ms) after the tip touches, reporting only pressure, then jumps 4–8 px to where
  the pen is. Seen in the raw kernel events (`getevent`), so every app on the tablet gets it. It costs
  about 20–40 ms at the start of each stroke and can leave a small hook. The positions during the
  hold are never measured, so it can't be recovered; drawing a smooth lead-in would be cosmetic.
- Windows takes whole-pixel pen positions; apps that draw without anti-aliasing (Paint's pencil)
  show 1 px steps. Ink-aware apps (OneNote) draw smoothly.

## How to measure

- Tablet overlay: tap the status in the pad; `adb logcat -s Inkbridge:D` prints the figures.
- Host: `RUST_LOG=info,ib_host::server=debug` logs the video link every 2 s;
  `RUST_LOG=ib_host::pen_samples=trace` logs every injected pen sample.
- Raw pen events on the tablet: `adb shell getevent -lt -c 4000 /dev/input/event3`
  (the stylus device; find it with `getevent -pl`).

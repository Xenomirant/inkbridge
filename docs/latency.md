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

Still to measure: Windows over Wi-Fi (QUIC), the Intel iGPU, a Linux host.

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

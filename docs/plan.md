# Inkbridge — Detailed Project Plan

## Context
The user has a Lenovo Xiaoxin Pad 2024 (TB331FC, Android 15, official ZUXOS since 2026-11-16) with a Lenovo Tab Pen Plus, a Windows 11 PC (RTX 5060 + Intel iGPU, campus network, WSL2) and a MacBook; an iPad will be borrowed later. Existing "tablet as pen display" tools are paid/subscription (Duet, SuperDisplay, VirtualTablet), closed, need drivers/adb, or lack pen pressure (Weylus: Linux-only pressure; Spacedesk: no pressure). Goal: a free **GPL-3.0** app, **Inkbridge**, that streams the desktop to the tablet by default and returns **native pressure/tilt pen input**, over **Wi-Fi or USB**, with **no admin rights, drivers or adb**, on Windows/macOS/Linux, Android first and iPad next, later adding **handwriting→text with correction**. **Latency is the #1 product metric.**

Already in place: `~/projects/inkbridge` (git initialised, nothing committed) with `README.md`, `docs/architecture.md` (draft v0.1), ADRs 0001 (shared Rust core via UniFFI), 0002 (IP-only transports, USB via tethering — tablet verified RNDIS-only), 0003 (QUIC). These get updated by this plan, not replaced.

## Decisions (confirmed or decided from research, 2026-11)
| Area | Decision | Why |
|---|---|---|
| Licence | **GPL-3.0-or-later** + an **App Store additional permission** (GPLv3 §7) on our own code; DCO sign-off for contributors | Lets us reuse Sunshine's tested GPL code (Windows pen injection). The §7 exception keeps an iPad App Store build possible; GPL-only third-party code must stay out of the iPad target. |
| First release | **Android tablet + Windows host**; then Linux → macOS → iPad | The user's setup; Windows pen API is the most complete, and Windows Ink gives free handwriting recognition. |
| Video | **Native OS encoders**, zero-copy GPU path: Media Foundation (Win), VideoToolbox (mac), VA-API (Linux); **openh264** software fallback; H.264 by default (universal hardware decode), HEVC optional; optional direct **NVENC** backend only if measurements justify it | Lowest overhead, full control of low-latency settings, no 30–80 MB FFmpeg bundle or version churn. |
| Capture | Per-OS crates: `windows-capture` (WGC + DXGI fallback for older Windows), `screencapturekit` (mac 12.3+, CGDisplayStream fallback), `ashpd` + `pipewire` (Wayland, restore tokens), XShm (X11) | Only per-OS backends give GPU textures; cross-platform crates (xcap/scap) are CPU-copy or slowly maintained. |
| Transport | `quinn` QUIC (datagrams for pen, streams for control/video) + `mdns-sd` discovery + **TCP/TLS fallback** for networks that block UDP. iroh 1.0 noted as a future option for cross-network use | Mature, LAN-first; the fallback covers campus/corporate UDP blocking. |
| Pairing | QR = host addresses + cert fingerprint + 128-bit one-time secret → mutual pinned TLS (no PAKE needed when the secret travels in the QR) | Simple, auditable, secure. |
| USB | Android **USB tethering** (RNDIS: Windows/Linux built-in drivers). iPad: **usbmuxd** (Apple Mobile Device Support on Windows, which the free Apple Devices app installs; built-in on macOS; libimobiledevice on Linux) | No drivers we ship; documented one-time user steps. |
| Mobile core | Shared Rust core via **UniFFI 0.32** (pin the version; pre-1.0) — Kotlin now, Swift later; `cargo-ndk` | One protocol implementation; native decode/render/pen on each platform. |
| Desktop UI | **Tauri 2** (tray + settings + pairing QR) over `ib-host` | Small, cross-platform, Rust-native. |
| Android minimum | **API 26** (Android 8) runs; low-latency features enabled at runtime: front-buffered ink API 29+, `KEY_LOW_LATENCY` decode API 30+ | Supports older tablets without penalising new ones. |
| Linux pen | uinput (full pressure) as an **opt-in** one-time udev rule; default no-root path = RemoteDesktop portal + libei (`reis`) pointer-only, switching to stylus once **libei ≥1.7** + compositor support lands | Respects "no admin" by default. |
| macOS pen | Post CGEvent tablet pointer + proximity events (Accessibility permission). Avoid virtual HID (CoreHID pressure bug FB24785043 on macOS 26) | The only working driverless path; app compatibility gets tested. |
| Handwriting | Pluggable `Recognizer`; tier 1 = OS recognisers (Windows Ink, iPadOS `PKStrokeRecognizer`; ML Kit optional, unreliable in China); tier 2 = bundled open models on **tract** (pure Rust, older CPUs) — Zinnia/Tegaki for CJK characters + our own small LSTM-CTC line model; correction UI first | No open multilingual stroke model exists; datasets are mostly non-commercial → weights under a separate licence. |

## Latency strategy (designed in from M0)
Target pen-to-photon: **≤ 50 ms over Wi-Fi, ≤ 35 ms over USB**; perceived ink latency **≈ 0** thanks to local prediction.
1. **Local ink first:** the tablet draws the stroke immediately (front-buffered renderer + `MotionEventPredictor`; on iPad, predicted touches), then hands over to the real desktop pixels when the frame carrying that ink arrives. This hides all the round-trip time.
2. **Pen path:** sent the moment it's sampled (`requestUnbufferedDispatch`, historical samples batched per packet), QUIC datagrams, injected on the host on a dedicated high-priority thread (no async executor hop).
3. **Video path:** capture on change only; GPU texture → encoder with no CPU copy; encoder settings: no B-frames, zero lookahead, `MaxFrameDelay=0`, intra-refresh instead of large keyframes, slice output; one frame in flight; the decoder presents immediately (`KEY_LOW_LATENCY`, no jitter buffer on the LAN); dirty-rectangle/region streaming of the area being written on.
4. **Measurement is part of the product:** every packet carries timestamps; a clock-offset handshake gives per-stage latency (sample → send → inject → capture → encode → send → decode → present). Shown in a debug overlay and saved as CI benchmark artefacts. A `tools/latency-probe` "photon test" (flash a region, detect it on the tablet) gives the end-to-end ground truth.
5. **Adaptive quality:** bitrate and resolution adapt to RTT and loss; when they conflict, latency is protected over sharpness.

## Repository layout (Cargo workspace + mobile apps)
```
crates/ib-protocol     wire types (postcard), versioning, capabilities — no I/O
crates/ib-geometry     tablet↔region↔desktop mapping, DPI, multi-monitor — pure
crates/ib-ink          stroke model, gestures (scratch-out/join/split), Recognizer trait
crates/ib-clock        clock sync + latency accounting
crates/ib-transport    quinn + TCP/TLS fallback, mdns-sd, pairing/pinning, link selection (USB preferred)
crates/ib-session      session state machine (shared by host and clients)
crates/ib-capture      trait ScreenSource + backends {windows, macos, wayland, x11, fake}
crates/ib-encode       trait VideoEncoder + {mf, videotoolbox, vaapi, openh264, (nvenc)} 
crates/ib-inject       trait PenSink + {win_pointer, mac_cgevent, linux_uinput, linux_libei, fake}
crates/ib-recognize    recognisers {windows_ink, tract_models, zinnia} (M5)
crates/ib-host         composition root + pipeline threads
crates/ib-ffi          UniFFI surface for Kotlin/Swift
apps/desktop           Tauri 2 app
apps/android           Kotlin, Compose, MediaCodec, graphics-core, input-motionprediction
apps/ios               Swift/SwiftUI, VideoToolbox, UIKit pencil handling (M4)
tools/fake-tablet      CLI client for tests/CI; tools/latency-probe
docs/                  architecture.md, adr/, protocol.md, permissions.md, latency.md
```
Dependency rule: `apps → ib-host → backends → session/transport → protocol/geometry/ink/clock`. Pure crates never depend on OS or I/O.

## Engineering practices
- Rust stable pinned (`rust-toolchain.toml`), edition 2024, `rustfmt`, `clippy -D warnings`, `cargo-deny` (licences must be GPL-3-compatible, plus advisories), `cargo-machete`; `thiserror` in libraries, `anyhow` in binaries; `tracing` logs.
- Every OS boundary sits behind a trait with a **fake** backend → E2E loopback test (fake capture → real encode → real transport → fake tablet → fake inject) runs on all CI OSes.
- `proptest` (geometry, protocol round-trips), `cargo-fuzz` (decoders), criterion benchmarks for the pipeline stages.
- Android: Kotlin, ktlint + detekt, unit and instrumented tests; iOS: SwiftLint, XCTest.
- CI: GitHub Actions matrix (Windows, macOS, Ubuntu, Android emulator build), a release workflow producing signed artefacts. Mirror to **Gitee** for users in China.
- Process: conventional commits, DCO, CHANGELOG via git-cliff, semver for the app + separate protocol version, an ADR for every costly decision, `CONTRIBUTING.md`, `SECURITY.md`.

## Milestones and tasks
**M0 — Foundations (repo, protocol, CI)**
- Install Rust via rustup (user-level, WSL) + `cargo-ndk`, Android SDK/NDK command-line tools in WSL; Windows targets are built natively on Windows (rustup on Windows, MSVC build tools — the one install that may need the user's consent).
- Workspace skeleton, `ib-protocol` v0, `ib-geometry`, `ib-clock`, fake backends, `tools/fake-tablet`, CI matrix, licence files (GPL-3.0 + §7 App Store permission), `docs/protocol.md`.
- Update `docs/architecture.md` + ADRs with this plan's decisions; add ADR-0004 (licence), 0005 (native encoders), 0006 (latency architecture), 0007 (recognition tiers).

**M1 — Pen-only over Wi-Fi: Android → Windows**
- `ib-transport` (quinn + mDNS + QR pairing + TCP fallback); `ib-session`.
- Windows `PenSink` via `CreateSyntheticPointerDevice(PT_PEN)` + `InjectSyntheticPointerInput` (pressure, tilt, rotation, eraser, barrel button; Sunshine's `pen_update` as reference); `SendInput` mouse fallback.
- Android app: pairing via QR scan, stylus capture with historical samples, unbuffered dispatch, blank "drawing pad" mode.
- Exit: pressure strokes in Krita, OneNote and Windows Ink-to-Text; pen-to-inject latency logged < 10 ms on the LAN.

**M2 — Screen streaming + local ink (Windows)**
- `ib-capture` windows-capture (WGC, DXGI fallback), `ib-encode` Media Foundation H.264 zero-copy (D3D11), openh264 fallback; region/monitor selection.
- Android decode (`MediaCodec` + SurfaceView, `KEY_LOW_LATENCY` when available), front-buffered local ink + motion prediction, adaptive bitrate, latency overlay, `latency-probe`.
- Exit: ≤ 50 ms pen-to-photon over Wi-Fi on the user's PC and tablet; stable 60 fps; works on the Intel iGPU too (older-hardware proxy).

**M3 — USB + Linux host**
- USB tethering link detection + automatic preference; route-hijack detection and guidance; Android deep-link to the tethering setting.
- Linux: Wayland capture (ashpd + pipewire, restore tokens), X11 capture, VA-API encode, uinput opt-in helper (generates the udev rule and explains it), libei pointer path.
- Exit: ≤ 35 ms over USB on Windows; Linux X11 pressure with uinput; Wayland pointer working.

**M4 — macOS host + iPad client**
- macOS: ScreenCaptureKit + VideoToolbox (real-time, no reordering), CGEvent tablet injection, permission onboarding; test matrix of apps (Krita, Photoshop, Affinity, Preview/Notes, OneNote) for pressure support.
- iPad (when the borrowed device arrives): Swift app via the UniFFI Swift bindings; Pencil force/altitude/azimuth, coalesced + predicted touches, hover (M2+ iPads), Pencil Pro squeeze/roll; VideoToolbox decode; Wi-Fi + usbmuxd USB (peertalk-style framing); distribution via free 7-day provisioning for testing → TestFlight/App Store needs the $99/yr Apple programme (to decide later), plus AltStore PAL in the EU.
- Exit: feature-parity table green for Windows/macOS hosts and Android/iPad clients.

**M5 — Handwriting → text + correction**
- `ib-ink` gestures (scratch-out, join/split, strike-to-select), candidate list UI on the tablet, strokes linked to text for re-recognition.
- Recognisers: Windows Ink (strokes passed to the host), iPadOS `PKStrokeRecognizer`; bundled tract models: Zinnia/Tegaki (zh character mode) + our own LSTM-CTC line model (en/ru/zh) trained on IAM-OnDB/CASIA/synthetic Cyrillic (weights under a separate non-commercial-compatible licence, documented); n-gram re-ranking + user dictionary; optional math via a CoMER-style ONNX model.
- Text delivered through the host's keyboard injection path.

**M6 — Release**
- Windows per-user installer (no UAC, MSIX/NSIS per-user) + portable zip; macOS notarised dmg (signing costs to decide); Linux AppImage + Flatpak; Android APK on GitHub/Gitee Releases + F-Droid; Google developer verification registration (required for sideloading on certified devices from 2027 — one-time fee); auto-update check (no telemetry). Docs site, user guide, troubleshooting (firewall prompt, campus Wi-Fi isolation, tethering).

## Risks and mitigations
| Risk | Mitigation |
|---|---|
| Campus Wi-Fi blocks peer/multicast traffic (likely on the user's network) | USB tethering first-class; manual IP; TCP fallback; tablet-hotspot mode |
| macOS apps ignore CGEvent pressure; Apple HID bug | App test matrix; track FB24785043; document limitations |
| Wayland stylus depends on libei 1.7 + compositors | Pointer fallback now; uinput opt-in; upgrade path tracked in an ADR |
| UniFFI pre-1.0 breaking changes | Pin the version; keep the FFI surface small and coarse |
| GPL vs App Store | §7 permission on our code; no GPL-only third-party code in the iOS target; DCO |
| Recognition quality for zh/ru offline | OS recognisers first; ship the correction UI before model quality; community-contributed samples (opt-in) |
| Encoder variance across GPUs/older hardware | Runtime capability probing, openh264 fallback, per-vendor presets, measured on the iGPU + RTX |

## Verification
- **Per PR:** CI green on Windows/macOS/Linux + Android build; unit/property/fuzz-smoke tests; E2E loopback test with fake devices; clippy/fmt/deny.
- **Per milestone, on real hardware (the user's PC + TB331FC; later the MacBook + iPad):**
  - M1: draw pressure ramps in Krita; check pressure/tilt values in a pointer-event test page; Windows Ink-to-Text converts writing.
  - M2/M3: `latency-probe` end-to-end numbers recorded in `docs/latency.md` (Wi-Fi vs USB, RTX vs iGPU); 30-minute soak test with no drops or leaks.
  - Pairing security test: an unpaired device cannot connect or inject; replayed or forged packets are rejected.
- **Immediate next step after approval:** install rustup (user-level), commit the existing docs, apply M0 (skeleton, decisions into docs/ADRs, CI), then start M1.

# Inkbridge — Architecture

> Working title. Status: **draft v0.1** (2026-11-16). Decisions with lasting impact are recorded as ADRs in [`docs/adr/`](adr/).

## 1. What it is

Inkbridge turns a pen tablet (first target: Android tablets with an active stylus, e.g. Lenovo Xiaoxin Pad 2024 + Tab Pen Plus) into a **pen display for a desktop computer** running Windows, macOS or Linux:

- the desktop screen (or a chosen region/window) is **streamed to the tablet by default**;
- the pen's position, pressure, tilt, hover, buttons and eraser are **sent back and injected as native pen input** on the desktop;
- it works **over Wi-Fi or over a USB cable**;
- later: **handwriting → text** with error correction.

## 2. Goals and constraints

| # | Requirement | Consequence |
|---|---|---|
| G1 | Windows, macOS, Linux hosts | Platform code isolated behind traits; one shared core. |
| G2 | Screen streaming on by default | Low-latency hardware video encode/decode is a core subsystem, not an add-on. |
| G3 | Native pen input (pressure, tilt) | Per-OS injection backends; pen events are never "just mouse". |
| G4 | **No admin rights, no drivers, no adb by default; minimal system changes** | The default path uses only user-level OS APIs; unavoidable OS permissions are requested through the OS's normal consent prompt and explained in-app. Where a platform limit has **no user-level solution**, an opt-in **Advanced option** may use adb, a udev rule, vendor device software or an elevated helper — clearly labelled, never required for the basic experience. See §6. |
| G5 | Wi-Fi and USB | All transports are IP-based so one protocol stack serves both (ADR-0002). |
| G6 | Maintainable, transferable | Rust workspace, strict module boundaries, versioned protocol, CI on all platforms, docs + ADRs. |
| G7 | Secure by default | Pairing + mutual authentication + encryption. An unauthenticated pen-injection endpoint is equivalent to remote control of the PC and must never exist. |

**Non-goals (for now):** iPad client (kept possible by a platform-neutral protocol), internet/NAT traversal, multi-user sessions, audio streaming.

**Latency budget (pen-to-photon, target ≤ 50 ms on a good LAN, ≤ 35 ms over USB):**
pen sample → tablet send ≈ 2 ms · network ≈ 2–8 ms · host inject ≈ 1 ms · app redraw ≈ 8–16 ms · capture ≈ 2–8 ms · encode ≈ 3–6 ms · network ≈ 2–8 ms · decode + present ≈ 5–10 ms. The tablet also draws a **local predicted ink trail** so perceived latency is near zero (§5.4).

## 3. System overview

```
 ┌──────────────── Desktop host (Rust) ────────────────┐            ┌──────────── Tablet client (Android) ───────────┐
 │                                                     │            │                                                 │
 │  Capture ──► Encode (HW H.264/HEVC) ──┐             │  QUIC/TLS  │             ┌──► Decode (MediaCodec) ──► Surface│
 │  (WGC / SCK / PipeWire)               ├─► Session ◄─┼────────────┼─► Session ◄─┤                                   │
 │                                       │   (core)    │  Wi-Fi or  │   (core,    └─◄ Pen capture (MotionEvent) ◄── pen│
 │  Inject ◄── Coordinate mapper ◄───────┘             │  USB-IP    │    via FFI)        + local ink prediction       │
 │  (Win pointer / CGEvent / uinput…)                  │            │                                                 │
 │  UI: tray + settings window (Tauri)                 │            │  UI: Kotlin (Compose)                           │
 └─────────────────────────────────────────────────────┘            └─────────────────────────────────────────────────┘
```

The **same Rust core** (protocol, session state machine, transport, pairing) runs on both ends; Android calls it through generated Kotlin bindings (UniFFI). Platform-specific work (capture, encode, inject, decode, rendering, pen sampling) lives at the edges.

## 4. Repository layout

```
inkbridge/
├── Cargo.toml                 # workspace
├── crates/
│   ├── ib-protocol/           # wire types, versioning, codecs (no I/O)
│   ├── ib-transport/          # QUIC endpoints, discovery (mDNS), pairing, link selection
│   ├── ib-session/            # session state machine; glues streams together; platform-agnostic
│   ├── ib-capture/            # trait ScreenSource + backends (cfg-gated)
│   ├── ib-encode/             # trait VideoEncoder + backends (MF / VideoToolbox / VA-API / software)
│   ├── ib-inject/             # trait PenSink + backends (Windows / macOS / Linux tiers)
│   ├── ib-geometry/           # coordinate mapping, multi-monitor, rotation, aspect fitting
│   ├── ib-ink/                # stroke model; later: recognition interfaces (§8)
│   ├── ib-host/               # host composition root: wires capture→encode→session→inject
│   └── ib-ffi/                # UniFFI bindings exposing ib-session/transport to Android
├── apps/
│   ├── desktop/               # Tauri app (tray, pairing QR, settings), depends on ib-host
│   └── android/               # Gradle project; Kotlin UI, MediaCodec, pen capture; uses ib-ffi
├── tools/                     # dev tools: fake tablet (CLI), latency probe, protocol fuzz targets
├── docs/
│   ├── architecture.md        # this file
│   └── adr/                   # architecture decision records
└── .github/workflows/         # CI matrix
```

**Dependency rule:** arrows only point inward — `apps → ib-host → {capture, encode, inject, session} → {transport, protocol, geometry, ink}`. `ib-protocol`, `ib-geometry`, `ib-ink` have **no** OS dependencies and **no** I/O, so they are trivially testable and reusable (including by a future iPad client).

## 5. Subsystems

### 5.1 Protocol (`ib-protocol`)
- Messages are Rust enums serialised with **postcard** (compact, `no_std`-friendly). A `ProtocolVersion` + **capability negotiation** at handshake (`codecs`, `max_resolution`, `pen_features`, `transport`) so old/new clients interoperate.
- Channels on one QUIC connection:
  - **control** — reliable bidirectional stream (hello, config, region selection, keep-alive);
  - **pen** — **QUIC datagrams** (unreliable, unordered, lowest latency). Each packet carries a sequence number and a small batch of samples (including Android "historical" samples), so a lost packet costs ≤ 1 frame of ink; *pen-up/down and button transitions are also mirrored on the control stream* so state can never get stuck;
  - **video** — one unidirectional stream per keyframe interval (stale data dropped by resetting the stream instead of head-of-line blocking).
- Pen sample: `{t_us, x, y (normalised 0..1 of the streamed region), pressure 0..1, tilt_x, tilt_y, rotation, hover, buttons, tool: Pen|Eraser}`.

### 5.2 Transport, discovery, pairing (`ib-transport`)
- **QUIC** via `quinn` (TLS 1.3, congestion control, datagrams, multiplexing in one UDP port).
- **Discovery:** mDNS/DNS-SD `_inkbridge._udp`; manual IP entry as fallback (some campus/corporate Wi-Fi blocks multicast or client-to-client traffic — the app must detect and explain this).
- **Pairing:** host shows a **QR code** containing host address(es), port, host certificate fingerprint and a one-time pairing secret. The tablet scans it, connects, and both sides authenticate (pinned self-signed certificates + SPAKE2/secret confirmation). Paired devices are remembered; nothing connects without pairing.
- **USB** (ADR-0002): the user enables Android's built-in **USB tethering**; the tablet and PC then share a private IP link over the cable with OS-built-in drivers (RNDIS/NCM). The transport sees it as just another interface and **prefers it automatically** when present (lower latency, no Wi-Fi interference). Caveat to handle in UX: some OSes may route internet through the tethered link — we document the toggle (Android “share connection” off / data saver) and detect the situation.

### 5.3 Screen capture + encoding (`ib-capture`, `ib-encode`)

| OS | Capture (user-level) | Hardware encode | Permission |
|---|---|---|---|
| Windows 10 1903+ | Windows.Graphics.Capture | Media Foundation (NVENC/QSV/AMF via MF) | none (yellow capture border on some builds) |
| macOS 12.3+ | ScreenCaptureKit | VideoToolbox | Screen Recording (one-time OS prompt) |
| Linux Wayland | xdg-desktop-portal ScreenCast + PipeWire | VA-API (fallback: software) | portal dialog per session (can be persisted with restore token) |
| Linux X11 | XShm / XComposite | VA-API | none |

- Low-latency settings: no B-frames, CBR/VBR capped, short GOP + **intra-refresh**, encoder fed from GPU textures (zero-copy where possible), frame pacing on damage (static screen → no frames).
- Software fallback: OpenH264 (Cisco binary, patent-licensed) loaded at runtime.
- Default: stream the display that the region maps to, scaled to the tablet's resolution and aspect; user can pick a region or a single window.

### 5.4 Tablet client (`apps/android`)
- Kotlin + Jetpack Compose UI; **SurfaceView** with `MediaCodec` low-latency decode (`KEY_LOW_LATENCY`), presented as soon as decoded.
- Pen capture from `MotionEvent` (`TOOL_TYPE_STYLUS/ERASER`, `AXIS_PRESSURE`, `AXIS_TILT`, `AXIS_ORIENTATION`, hover events, `getHistorical*` for full sample rate); `requestUnbufferedDispatch` to avoid 60 Hz batching.
- **Local ink prediction:** draw the stroke immediately on a transparent overlay (Android `androidx.input` motion prediction + front-buffered rendering), and fade it once the real desktop frame containing that ink arrives. This hides network + encode latency.
- Palm rejection: only stylus pointers are forwarded in pen mode; finger gestures map to scroll/zoom of the view (configurable).
- Minimum Android 11 (API 30), so low-latency decode and front-buffered rendering are always available.
- No root, no adb, distributed as a normal APK (store + direct download, as Google Play isn't available on China-market tablets).

### 5.5 Pen injection (`ib-inject`)
`trait PenSink { fn caps(&self) -> PenCaps; fn emit(&mut self, s: &PenSample, target: Point) -> Result<()>; }` — selected at runtime by capability probing, best tier first.

| OS | Tier 1 (preferred) | Fallback | Admin? |
|---|---|---|---|
| Windows | `InjectSyntheticPointerInput` with `PT_PEN` — real pen with pressure/tilt/eraser; Windows Ink & handwriting work | `SendInput` mouse | **No.** (UIPI: can't drive elevated windows unless host is elevated — documented.) |
| macOS | `CGEvent` mouse events with tablet sub-type (pressure, tilt) + proximity events | plain mouse | No; needs **Accessibility** permission (one-time OS prompt). True virtual HID would need DriverKit entitlement — out of scope. |
| Linux | `uinput` virtual tablet (`BTN_TOOL_PEN`, `ABS_PRESSURE`, tilt) — works on X11 and Wayland | libei / RemoteDesktop portal (no root; pointer-only until tablet support lands) · X11 XTest | uinput needs write access to `/dev/uinput` (one-time udev rule — offered, never required). |

### 5.6 Geometry (`ib-geometry`)
Normalised tablet coordinates → streamed region → desktop global coordinates, handling multi-monitor layouts, DPI scaling, rotation and letterboxing. Pure functions, exhaustively unit/property tested.

### 5.7 Host app (`apps/desktop`)
Tauri (Rust backend + small web UI): tray icon, pairing QR, display/region picker, transport status (Wi-Fi/USB, latency), permission checklist with "why" explanations. Runs as a normal user process; optional autostart via per-user mechanisms only.

## 6. Permissions matrix (the "no scary setup" promise)

| | Windows | macOS | Linux |
|---|---|---|---|
| Install | per-user installer (no UAC) | drag-to-Applications, notarised | AppImage / Flatpak |
| Screen | — | Screen Recording prompt | portal dialog (Wayland) |
| Input | — | Accessibility prompt | optional udev rule for pressure; otherwise reduced features |
| Firewall | per-user firewall prompt may appear for UDP listen | — | — |
| Admin / drivers / adb | **not required for the default path** | **not required for the default path** | **not required for the default path** |

**Advanced options** (opt-in, labelled "Advanced", each with an in-app explanation of what it changes and how to undo it). They exist only where the default path hits a platform limit:

| Option | Overcomes | Platforms |
|---|---|---|
| adb port forwarding over USB | No NCM tethering (e.g. Android tablet → Mac over USB); networks blocking all peer traffic | Android client, any host |
| uinput udev rule (one-time, needs root) | No pressure/tilt on Linux without uinput (until libei stylus support lands) | Linux host |
| Apple Mobile Device Support (via the free Apple Devices app) | iPad over USB on Windows | Windows host + iPad |
| Elevated helper / uiAccess build | Pen input into elevated (admin) windows (UIPI) | Windows host |
| Firewall rule / interface-metric fix | Campus firewalls, tethering hijacking the default route | Windows/Linux host |

## 6a. Platform compatibility

Every client works with every host; the protocol doesn't depend on platform. Wi-Fi works for every pair. USB differs:

| | Wi-Fi | USB (default) | USB (Advanced) | Pen pressure |
|---|---|---|---|---|
| Android → Windows | ✅ | ✅ USB tethering (RNDIS) | adb | ✅ full |
| Android → Linux | ✅ | ✅ USB tethering | adb | ✅ with uinput option / pointer-only otherwise |
| Android → macOS | ✅ | ✅ only if the device tethers via NCM (e.g. Pixel 6+) | **adb** (needed for RNDIS-only tablets such as TB331FC) | ⚠️ app-dependent (CGEvent) |
| iPad → macOS | ✅ | ✅ usbmuxd (built in) | — | ⚠️ app-dependent |
| iPad → Windows | ✅ | ✅ once Apple Devices app is installed | — | ✅ full |
| iPad → Linux | ✅ | ✅ libimobiledevice/usbmuxd | — | as Linux above |

## 7. Engineering practices
- **Rust:** stable toolchain pinned via `rust-toolchain.toml`; `rustfmt` + `clippy -D warnings`; `thiserror` in libraries, `anyhow` only in binaries; `tracing` for structured logs; `tokio` runtime confined to `ib-transport`/`ib-host`.
- **Boundaries:** every OS integration behind a trait with a **fake implementation** used in tests; `#[cfg(target_os = …)]` only inside backend modules.
- **Tests:** unit tests per crate; `proptest` for geometry and protocol round-trips; `cargo fuzz` targets for message decoding; an **end-to-end loopback test** (fake capture → real encode/transport → fake tablet in `tools/`) runs in CI on all three OSes; latency probe tool for manual benchmarks.
- **Android:** Kotlin, ktlint/detekt, unit tests + instrumented tests for the pen pipeline; UniFFI bindings generated in the Gradle build.
- **CI:** GitHub Actions matrix (windows-latest, macos-latest, ubuntu-latest + Android build); `cargo-deny` for licences/advisories; release builds signed (Windows Authenticode, Apple notarisation — both have yearly costs).
- **Process:** small PRs, conventional commits, semver for app + protocol version, CHANGELOG, ADR for every decision that is expensive to reverse.

## 8. Handwriting → text (later)
Kept out of the first milestones, but designed for now:
- `ib-ink` stores strokes as vector data (points + pressure + time), independent of the video path, so recognition never depends on pixels.
- `trait Recognizer { fn recognize(&self, strokes: &[Stroke], hints: &Context) -> Vec<Candidate>; }` with candidates + confidence, enabling **error correction UI** (pick alternative, scratch-out to delete, insert caret gestures).
- Candidate backends: on-tablet ML (e.g. ML Kit Digital Ink — note Google-service availability on China-market tablets), OS recognisers (Windows Ink handles this natively once pen injection works), or a bundled open model (ONNX) for macOS/Linux parity.
- Output: typed into the focused desktop app via the injection layer's keyboard path.

## 9. Milestones
| | Scope | Exit criterion |
|---|---|---|
| M0 | Workspace, protocol, CI, fake tablet tool | CI green on 3 OSes |
| M1 | Pen-only over Wi-Fi (no video), Windows + Linux(uinput), pairing | Pressure-sensitive strokes in Krita/OneNote |
| M2 | Screen streaming (Windows first), local ink prediction | ≤ 50 ms pen-to-photon on LAN |
| M3 | USB tethering transport + auto-preference | ≤ 35 ms over USB |
| M4 | macOS host (SCK + CGEvent), Linux Wayland capture | Feature parity table green |
| M5 | Handwriting → text + correction UI | Usable for note-taking |
| M6 | Packaging, signing, auto-update, docs site | Public beta |

## 10. Open questions
1. ~~Licence model~~ — **resolved:** GPL-3.0-or-later + App Store permission (ADR-0004).
2. ~~iPad client~~ — **resolved:** in scope for M4 (a borrowed iPad for testing); protocol and core are shared.
3. ~~Product name~~ — **resolved:** Inkbridge.
4. ~~Encoding approach~~ — **resolved:** native OS encoders (ADR-0005).
5. Distribution channel for the Android app in China (no Google Play on PRC tablets): direct APK, Lenovo/Huawei/Xiaomi stores, F-Droid?

## 11. Risks
| Risk | Mitigation |
|---|---|
| Campus/corporate Wi-Fi blocks peer traffic or multicast | Detect + recommend USB tethering; manual IP; hotspot mode |
| USB tethering steals the PC's default route | Detect; guide user to disable tablet's upstream sharing; prefer link-local addresses |
| macOS apps ignoring CGEvent tablet pressure | Test matrix of popular apps; document; future DriverKit path if commercial |
| Linux Wayland fragmentation | Tiered backends; portal-first; uinput opt-in |
| Encoder latency/quality varies per GPU | Per-vendor presets, runtime fallback, telemetry-free local latency probe |

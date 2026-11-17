# Inkbridge — notes for Claude Code

Inkbridge turns a pen tablet into a pen display for a desktop: it streams the screen to the tablet and
injects the tablet's pen input (pressure, tilt) on the host. GPL-3.0-or-later with an App Store
exception (`LICENSE-APPSTORE-EXCEPTION.md`). Read before changing things:

- `docs/plan.md` — goals, decisions, milestones (M0–M6). `docs/status.md` — where we are now.
- `docs/architecture.md`, `docs/protocol.md`, `docs/adr/` — design and the reasons behind it.
- `docs/linux.md` — Linux host details (Wayland portal, uinput, VA-API, validation kit).

## Product rules (from the user)
- **Latency is the #1 priority.** Measure, don't guess; protect latency over sharpness.
- **Default path needs no admin rights, drivers or adb.** Tools like adb, root, udev rules or uinput
  are allowed only as opt-in "Advanced" options when no user-space way exists.
- Native OS encoders (Media Foundation, VideoToolbox, VA-API) for lowest latency; openh264 fallback.
- Android client minimum: Android 11 (API 30). iPad client later (M4).
- Free, maintainable, best practices; handwriting-to-text comes later (M5).

## Engineering conventions
- Rust stable pinned in `rust-toolchain.toml`, edition 2024. Workspace lints are strict: clippy
  pedantic with `-D warnings` — run `cargo clippy --workspace --all-targets` (and with
  `--no-default-features` on Linux) before committing. `cargo fmt --all`. `cargo deny check`.
- `thiserror` in libraries, `anyhow` in binaries, `tracing` for logs. Every OS boundary sits behind a
  trait with a fake backend (`ScreenSource`, `VideoEncoder`, `PenSink`) so pipelines are testable.
- Unsafe FFI: each `unsafe` block gets a `// SAFETY:` comment.
- Conventional commits, signed off (`git commit -s`), small focused commits; ADR for costly decisions.
- Tests for every change; hardware-dependent tests are `#[ignore]`d with how to run them in the doc comment.

## Layout
`crates/` — ib-protocol, ib-geometry, ib-clock, ib-transport (QUIC), ib-session, ib-capture,
ib-encode, ib-inject, ib-portal (Linux Wayland portal), ib-host (binary `inkbridge-host`), ib-ffi (UniFFI).
`apps/android` — Kotlin client. `tools/` — fake-tablet, pen-test page, linux-validation kit.

## Build and test
- `cargo test --workspace`; host: `cargo run -p ib-host -- --help`.
- Android: `apps/android` (Gradle; Rust core via cargo-ndk).
- The user's devices: Windows 11 PC (RTX 5060 + Intel iGPU, WSL Ubuntu 26.04 for development),
  Lenovo Xiaoxin Pad 2024 (TB331FC, Android 15) with Lenovo Tab Pen Plus, a MacBook; iPad later.
- On the campus network, Anthropic's API needs the university VPN.

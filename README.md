# Inkbridge

Use a pen tablet as a pen display for Windows, macOS and Linux — over Wi-Fi or a USB cable, with native pressure- and tilt-sensitive pen input, and without admin rights, drivers or adb on the default path. Free and open source (GPL-3.0-or-later), no subscriptions.

**Status:** usable on Windows and macOS with an Android tablet (tested with a Lenovo Xiaoxin Pad
2024); Linux is implemented but not yet tried on a real desktop. No installers yet: build from source.
See [status](docs/status.md) and the [plan](docs/plan.md).

The tablet shows the computer's screen and its pen draws there with pressure and tilt. Two-finger
scrolling and zoom, the tablet's keyboard types on the computer, and the tablet can show any of the
computer's displays or become an extra one.

| | Windows | macOS | Linux |
|---|---|---|---|
| Pen | pressure, tilt, eraser, barrel button | pressure, tilt, rotation, eraser, barrel button | pressure with uinput (Advanced, one admin prompt); else a plain pointer (Wayland portal) |
| Screen | Media Foundation (hardware) | VideoToolbox (hardware) | VA-API; X11 or the Wayland portal |
| Wi-Fi | yes | yes | yes |
| USB cable | tethering, or adb (Advanced) | adb (Advanced): macOS has no RNDIS | tethering, or adb (Advanced) |
| Choose a display | yes | yes | main display only |
| Tablet as an extra screen | Advanced: a virtual display driver (one admin prompt) | yes, nothing to install | not yet |
| Keyboard from the tablet | yes | yes | not yet |
| Tray / menu-bar app | yes | yes | builds, not yet tried |
| Permissions | none | Screen Recording, Accessibility (asked when needed) | screen sharing portal |
| Screen latency (median) | 30 ms over USB | 34 ms over Wi-Fi, 33 ms over USB | not measured |

Latency figures and how they were measured: [docs/latency.md](docs/latency.md). Client: Android 11
or later; iPad later.

## Documentation
- [Architecture](docs/architecture.md) — design, permissions, platform compatibility
- [Plan](docs/plan.md) — milestones M0–M6
- [Status](docs/status.md) — what works, what was checked, open issues
- [Latency](docs/latency.md) — measured figures
- [Protocol](docs/protocol.md) — wire format
- [Decision records](docs/adr/)
- [Contributing](CONTRIBUTING.md) · [Security](SECURITY.md)

## Building
```sh
cargo test --workspace                 # toolchain version is pinned in rust-toolchain.toml
cargo run --release -p ib-host -- --help   # the host from the command line
cd apps/desktop && cargo run --release     # the tray / menu-bar app
cd apps/android && ./gradlew assembleDebug # the Android app (needs the NDK and cargo-ndk)
```
Use release builds when judging latency; a debug build makes the software encoder about 30 times
slower.

## Licence
GPL-3.0-or-later, with an additional permission for app-store distribution — see [`LICENSE`](LICENSE) and [`LICENSE-APPSTORE-EXCEPTION.md`](LICENSE-APPSTORE-EXCEPTION.md).

<p align="center">
  <img src="apps/desktop/icons/128x128@2x.png" width="96" alt="Inkbridge">
</p>

<h1 align="center">Inkbridge</h1>

<p align="center">
  Turn an Android tablet into a pen display for your computer.<br>
  Pressure and tilt, about 30 ms latency, over Wi-Fi or USB. Free and open source.
</p>

<p align="center">
  <a href="https://github.com/Xenomirant/inkbridge/actions/workflows/ci.yml"><img src="https://github.com/Xenomirant/inkbridge/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-GPL--3.0-blue" alt="License: GPL-3.0"></a>
  <img src="https://img.shields.io/badge/computer-Windows%20%7C%20macOS%20%7C%20Linux-informational" alt="Windows, macOS, Linux">
  <img src="https://img.shields.io/badge/tablet-Android%2011%2B-3ddc84" alt="Android 11+">
</p>

The tablet shows your computer's screen, and its pen works there like a real pen: pressure, tilt,
eraser and button, in any app that supports one. No drivers, admin rights or developer mode, no
account, no subscription.

## Features
- **Native pen input** with pressure and tilt
- **Low latency**: hardware video encoding on the computer, hardware decoding on the tablet
- **Wi-Fi or USB cable**: a session moves onto the cable as soon as it's plugged in
- **Extra screen**: use the tablet as another display, or show any of your displays
- **Keyboard and gestures**: type from the tablet, scroll and zoom with two fingers
- **Private**: pairing by QR code, encrypted connections, no cloud

## Supported platforms

| | Windows 10/11 | macOS 12.3+ | Linux |
|---|---|---|---|
| Pen pressure and tilt | ✓ | ✓ | ✓ with the uinput option¹ |
| Wi-Fi / USB cable | ✓ / ✓ | ✓ / with adb¹ | ✓ / ✓ |
| Extra screen | ✓ with a display driver¹ | ✓ | ✓ on GNOME |
| Keyboard from the tablet | ✓ | ✓ | ✓ |
| Screen latency (median) | 30 ms | 33 ms | 36 ms |

¹ Optional setup, off by default.
How latency is measured: [docs/latency.md](docs/latency.md). iPad support is planned.

## Getting started
Prebuilt downloads come with the first release; until then, [build from source](#building).

1. Start **Inkbridge** on the computer. It lives in the tray (menu bar on macOS).
2. Click **Pair a tablet** and scan the QR code with the Inkbridge app on the tablet.
3. Draw. Next time, tap the computer in the app; with the USB cable and USB tethering on, the
   tablet connects by itself.

## Building
Needs Rust (the version is pinned in `rust-toolchain.toml`); the tablet app also needs the Android
SDK, NDK and `cargo-ndk`.

```sh
cd apps/desktop && cargo run --release        # the desktop app
cd apps/android && ./gradlew assembleDebug    # the Android app
cargo test --workspace                        # the tests
```

On Linux, install the development packages listed in [the CI workflow](.github/workflows/ci.yml).

## Contributing
Issues and pull requests are welcome; see [CONTRIBUTING.md](CONTRIBUTING.md). Report security
problems privately, as [SECURITY.md](SECURITY.md) describes. Design notes are in [docs/](docs/):
[architecture](docs/architecture.md), [protocol](docs/protocol.md), [status](docs/status.md) and
[decision records](docs/adr/).

## License
[GPL-3.0-or-later](LICENSE), with an [additional permission](LICENSE-APPSTORE-EXCEPTION.md) for
app-store distribution.

# Inkbridge

Use a pen tablet as a pen display for Windows, macOS and Linux — over Wi-Fi or a USB cable, with native pressure- and tilt-sensitive pen input, and without admin rights, drivers or adb on the default path. Free and open source (GPL-3.0-or-later), no subscriptions.

**Status:** M0 (foundations) — protocol, geometry, clock-sync and backend interfaces are in place; nothing usable yet. See the [plan](docs/plan.md).

| Clients | Hosts |
|---|---|
| Android (first), iPad (later) | Windows (first), Linux, macOS |

## Documentation
- [Architecture](docs/architecture.md) — design, permissions, platform compatibility
- [Plan](docs/plan.md) — milestones M0–M6
- [Protocol](docs/protocol.md) — wire format
- [Decision records](docs/adr/)
- [Contributing](CONTRIBUTING.md) · [Security](SECURITY.md)

## Building
```sh
cargo test --workspace     # toolchain version is pinned in rust-toolchain.toml
```

## Licence
GPL-3.0-or-later, with an additional permission for app-store distribution — see [`LICENSE`](LICENSE) and [`LICENSE-APPSTORE-EXCEPTION.md`](LICENSE-APPSTORE-EXCEPTION.md).

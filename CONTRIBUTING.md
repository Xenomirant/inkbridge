# Contributing to Inkbridge

Thanks for helping! Read [`docs/architecture.md`](docs/architecture.md) and [`docs/plan.md`](docs/plan.md) first.

## Ground rules
- **Latency is the main product metric.** A change that adds latency to the pen or video path needs numbers (`tools/latency-probe` or the stage stats in `ib-clock`) in the PR.
- **The default path never needs admin rights, drivers or adb.** Features that need them are opt-in **Advanced options** (architecture §6).
- **Platform code stays at the edges.** OS-specific code lives only inside backend modules behind the `PenSink`, `ScreenSource` and `VideoEncoder` traits, gated with `#[cfg(target_os = …)]`. Pure crates (`ib-protocol`, `ib-geometry`, `ib-clock`, `ib-ink`) never do I/O or touch the OS.
- **Every backend has a fake** so the pipeline is testable in CI without hardware.
- **Decisions that are expensive to reverse get an ADR** in `docs/adr/`.

## Workflow
1. `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace` must pass locally.
2. Commits follow [Conventional Commits](https://www.conventionalcommits.org/) (`feat(inject): …`, `fix(protocol): …`, `docs: …`).
3. Every commit is signed off under the [Developer Certificate of Origin](https://developercertificate.org/): `git commit -s`.
4. Wire-format changes bump `PROTOCOL_VERSION` (minor for additions, major for breaking changes) and update `docs/protocol.md`.

## Licensing
Inkbridge is GPL-3.0-or-later with an App Store additional permission (`LICENSE-APPSTORE-EXCEPTION.md`). By signing off, you agree that your contribution is offered under both. Third-party GPL code without that permission (for example code adapted from Sunshine) may be used in **desktop host** crates only, never in code linked into the iOS client; mark such files with their origin and licence header.

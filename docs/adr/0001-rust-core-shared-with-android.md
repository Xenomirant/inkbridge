# ADR-0001: One Rust core shared by host and tablet

- Status: Accepted
- Date: 2026-11-16

## Context
The host must run on Windows, macOS and Linux; the tablet client on Android (possibly iPad later). Protocol, session state, pairing and transport logic would otherwise be written twice (Rust + Kotlin) and drift apart.

## Decision
- All protocol, session, transport and pairing logic lives in Rust crates (`ib-protocol`, `ib-transport`, `ib-session`).
- The Android app links these through **UniFFI**-generated Kotlin bindings (`ib-ffi`).
- Platform-specific work stays native: Kotlin for UI, `MediaCodec`, pen capture and rendering on Android; per-OS backends behind traits on the host.

## Consequences
- + Single source of truth for the wire protocol; compatibility bugs between ends become compile-time errors.
- + A future iOS client can reuse the same core through UniFFI's Swift bindings.
- − Android build needs the Rust NDK toolchain (`cargo-ndk`); CI must build both.
- − FFI boundary must be kept coarse-grained (no per-sample calls across FFI; pen samples are batched).

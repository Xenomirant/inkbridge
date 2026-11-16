# ADR-0004: GPL-3.0-or-later with an App Store additional permission

- Status: Accepted
- Date: 2026-11-16

## Context
Inkbridge is free software for users who don't want subscriptions. Existing tested code worth reusing is GPL (Sunshine: Windows pen injection with pressure/tilt; Moonlight). A permissive licence would forbid copying it and would allow closed paid forks. However, plain GPL is incompatible with Apple's App Store terms (the VLC precedent), and we want an iPad client.

## Decision
- Licence: **GPL-3.0-or-later** (`LICENSE`).
- Our own code additionally carries a **GPLv3 §7 permission** allowing distribution through app stores (`LICENSE-APPSTORE-EXCEPTION.md`).
- Contributors sign off under the DCO, agreeing to both.
- Third-party GPL code without that permission may only be used in **desktop host** crates, with an origin header, and must never be linked into the iOS client. `cargo-deny` checks licences; review checks origins.

## Consequences
- + We can reuse Sunshine's tested code where it saves real effort.
- + Forks must stay open.
- + An App Store build of the iPad client stays legally possible.
- − Contributors must accept the exception; changing it later requires every copyright holder's consent.
- − Some companies avoid GPL dependencies; that doesn't matter for an end-user app.

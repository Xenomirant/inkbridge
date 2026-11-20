# ADR-0008: The tablet as an extra screen on macOS uses the private `CGVirtualDisplay` API

- Status: Accepted
- Date: 2026-11-18

## Context
Showing a copy of a display is one use; the other is the tablet as a **second screen** that windows can be dragged onto. That needs a display the OS believes in. macOS has no public API for one. Options: a DriverKit display driver (needs an Apple entitlement and user approval in System Settings), a HDMI dummy plug (hardware), or `CGVirtualDisplay`, a private CoreGraphics API that other virtual-display apps use. It needs no driver, no admin rights and no extra permission, which matches the product rule for the default path.

## Decision
- `ib-capture::MacVirtualDisplay` creates the display with `CGVirtualDisplay`. Every class and selector is looked up at run time; a missing one is an error with a clear message, never a crash.
- Two kinds, chosen on the tablet. "Fill": the display has the shape of the tablet's drawing area; default mode two thirds of its pixels in points, drawn at 2x (1280x800 points, 2560x1600 pixels for the TB331FC), which leaves room for windows but is scaled twice on the way to the tablet. "Sharp": the display is exactly the picture the tablet's decoder takes (1920x1088), half of it in points at 2x, shown pixel for pixel with bars. A tablet whose decoder takes its whole screen gets one kind.
- The tablet chooses it in its Screen menu; `--extend` (with `--sharp`) starts that way. The display stays 15 s after a session ends, so a reconnecting tablet finds its windows in place.
- Capture, the pen and regions use the existing display selection (protocol 0.2); nothing in the protocol is new.

## Consequences
- + No driver, no admin rights; works today on macOS 27 (Apple silicon).
- − Apple can change or remove the API in any release. The feature then turns itself off (`extend_supported()` is false) and copying a display keeps working.
- − Rules out the Mac App Store for the host. The host is distributed directly (notarised bundle, M6), so this costs nothing now.
- − Core Graphics reads display modes once per process, so the host can't ask the pixel scale of a display that appeared after it started. For our own display the scale follows from the modes we set (2x wherever it fits the display's pixel limit). Choosing one of the hidden 1x variants in System Settings would make picture and pen disagree. The same limit means a monitor plugged in while the host runs is not listed until the host restarts.
- Windows needs an indirect display driver (admin rights: an "Advanced" option). Wayland desktops can create a virtual monitor through the screen-sharing portal. Both are follow-ups.

## Review (2026-11-20)
The project rule since then is to leave out features that can break with any OS release. This one was
reviewed against it and kept: macOS has no public way to add a display, the feature checks the API at
run time and disappears cleanly without it, and mirroring and choosing a real display don't depend on
it. A synthesized pinch gesture (undocumented `CGEvent` fields) was removed under the same rule, since
it had no such fallback value.

# ADR-0002: IP-only transports; USB via Android USB tethering

- Status: Accepted
- Date: 2026-11-16

## Context
Both Wi-Fi and USB-cable connections are required. A hard constraint is **no admin rights, no drivers, no adb** on the desktop.

Options considered for USB:

| Option | Admin/driver needed? | Notes |
|---|---|---|
| adb port forwarding | adb + developer mode on tablet | Scares users; disabled on some OEM ROMs. Not the default, but **offered as an Advanced option** (see architecture §6) because it's the only USB path for RNDIS-only tablets on macOS. |
| Android Open Accessory (AOA) via libusb | Windows needs WinUSB driver binding (admin) | Rejected. |
| **USB tethering (RNDIS/NCM)** | **No** — OS built-in class drivers on Windows, Linux, macOS (NCM) | Creates an ordinary IP link over the cable. |

## Decision
All transports are IP. Wi-Fi uses the LAN; USB uses Android's built-in USB tethering. The transport layer enumerates interfaces and automatically prefers the tethered link when it is available.

## Consequences
- + One protocol stack and one code path for both connection types; USB is "just a faster network".
- + Zero drivers, zero admin, zero adb.
- − The user must enable "USB tethering" on the tablet (one toggle; the app deep-links to the setting and explains it).
- − The desktop may start routing internet traffic through the tablet. Must be detected and explained; prefer link-local addressing.
- − macOS has no RNDIS driver; it needs the NCM variant. **Verified 2026-11-16 on the reference tablet (Lenovo TB331FC, ZUXOS 1.1.10.680 / Android 15): tethering is RNDIS-only** (`mUsbTetheringFunction: RNDIS`, `tetherableNcmRegexs: []`). So on macOS this tablet gets Wi-Fi only by default. Options to revisit for macOS-over-USB: (a) opt-in expert mode using adb port forwarding; (b) Wi-Fi Direct / tablet hotspot as a "private link" alternative; (c) per-model NCM support matrix.

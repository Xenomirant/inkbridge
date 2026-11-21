# Next steps after the Linux live-USB session (2026-11-21)

Read `docs/status.md` (M3 Linux block: verified, known issues, to do) and `docs/linux.md` first.

## In WSL (Android SDK there), in this order
1. Pen button: `StylusMapper.kt` forwards only BUTTON_STYLUS_PRIMARY/BUTTON_SECONDARY; the Tab Pen
   Plus button is BTN_STYLUS2 = BUTTON_STYLUS_SECONDARY. Add it (evidence in
   `docs/validation/2026-11-20-linux-live-usb/tilt-and-button.txt`).
2. Tilt X always 0: log AXIS_TILT/AXIS_ORIENTATION on the TB331FC; the hardware reports both tilt
   axes (+-60 deg), the app receives orientation 0. If Android/ZUI loses it, send tilt magnitude only.
3. Wi-Fi session -> USB: the app listens for the host's USB dial only while connecting
   (`Session.acceptUsb`); keep the listener during a Wi-Fi session, then add the host side
   (dial every 5 s, hand the connection to the accept loop; see status.md).
4. Linux kit: `run.sh` new checks (keyboard, extra screen, --adb); `dev-setup.sh`: export
   CARGO_HTTP_MULTIPLEXING=false (crates.io over HTTP/2 stalled on the campus network) and
   install the toolchain from rust-toolchain.toml.

## Linux, on a real install (not a live stick)
- The "stick" was a 64 GB card in a USB card reader; it failed (SQUASHFS read errors flooded
  syslog/kern.log until the RAM disk was full; then sudo/rm crashed). Use an installed system.
- No spare partition: the 1 TB NVMe is EFI + MSR + Windows C:. Options: shrink C: for dual boot
  (check BitLocker and free space first), or install Ubuntu on an external USB SSD.
- Then: desktop tray app (never run on Linux), re-measure fps/latency with the NVIDIA driver
  (the live session's ~31 fps cap came from NVK/zink read-back), two real monitors, non-US text.
- No-setup pointer/touch on the extra screen waits for flatpak/xdg-desktop-portal#2078.

## Network notes (campus VPN)
- The AnyConnect VPN is needed for Claude only: route just 160.79.104.0/23 via tun0 and delete
  the `default dev tun0` route; crates.io, rustup and GitHub work directly.

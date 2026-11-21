# Next steps after the Linux live-USB session (2026-11-21)

Read `docs/status.md` (M3 Linux block: verified, known issues, to do) and `docs/linux.md` first.

## In WSL (Android SDK there), in this order
1. ~~Pen button~~ done 2026-11-21: `StylusMapper.isBarrelPressed` also takes BUTTON_STYLUS_SECONDARY
   (the Tab Pen Plus's BTN_STYLUS2). Still to check with the pen on a host.
2. Tilt X always 0: measured 2026-11-21, Android/ZUI hands the app orientation 0 although its input
   reader has the direction (status.md). Only the angle arrives; the direction now comes from the drawing hand (done).
3. Wi-Fi session -> USB: the app listens for the host's USB dial only while connecting
   (`Session.acceptUsb`); keep the listener during a Wi-Fi session, then add the host side
   (dial every 5 s, hand the connection to the accept loop; see status.md).
4. Linux kit: `run.sh` new checks (keyboard, extra screen, --adb). `dev-setup.sh` sets
   CARGO_HTTP_MULTIPLEXING=false (done); rustup takes the toolchain from rust-toolchain.toml itself.

## Linux, on a real install (not a live stick)
- The "stick" was a 64 GB card in a USB card reader; it failed (SQUASHFS read errors flooded
  syslog/kern.log until the RAM disk was full; then sudo/rm crashed). Use an installed system.
- 2026-11-21: the user shrank C: and left 64 GB unallocated on the NVMe (BitLocker is off) for an
  Ubuntu 26.04 dual boot. After installing, `tools/linux-validation/desktops.sh` adds KDE Plasma
  (Wayland), Xfce (X11) and optionally Sway (wlroots), all chosen on SDDM's login screen.
- Then: desktop tray app (never run on Linux), re-measure fps/latency with the NVIDIA driver
  (the live session's ~31 fps cap came from NVK/zink read-back), two real monitors, non-US text.
- No-setup pointer/touch on the extra screen waits for flatpak/xdg-desktop-portal#2078.

## Network notes (campus VPN)
- The AnyConnect VPN is needed for Claude only: route just 160.79.104.0/23 via tun0 and delete
  the `default dev tun0` route; crates.io, rustup and GitHub work directly.

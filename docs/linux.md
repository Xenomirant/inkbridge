# Inkbridge on Linux

Status: **X11 sessions supported** (screen capture, software H.264, pen with pressure via uinput, USB tethering).
Wayland capture (screen-cast portal + PipeWire) and VA-API hardware encoding are next.

## Pen pressure: the uinput option (one-time, optional)

Linux delivers real pen input (pressure, tilt, eraser, buttons) to applications — Krita, GIMP, Xournal++,
browsers — from a *virtual tablet* created through `/dev/uinput`. That device node is root-only by
default, so Inkbridge needs a one-time permission:

```sh
inkbridge-host --setup-uinput        # asks for your password once (pkexec)
inkbridge-host --remove-uinput-setup # undo
```

What it changes, exactly:

| File | Content | Why |
|---|---|---|
| `/etc/udev/rules.d/60-inkbridge-uinput.rules` | `KERNEL=="uinput", SUBSYSTEM=="misc", TAG+="uaccess", OPTIONS+="static_node=uinput"` | Lets the **logged-in desktop user** (systemd-logind's `uaccess`, the same mechanism that grants your webcam) open `/dev/uinput`. Other users and remote logins get nothing. |
| `/etc/modules-load.d/inkbridge-uinput.conf` | `uinput` | Loads the uinput module at boot. |

Without it, Inkbridge still works but cannot create the virtual pen.

## USB connection

Turn on **USB tethering** on the tablet and open Inkbridge there. The host finds the tethering interface
(kernel driver `rndis_host` or `cdc_ncm`), reads the tablet's address from the routing table (it is the
interface's gateway) and connects to it — nothing to configure on Linux.

## Status table

| Feature | X11 | Wayland |
|---|---|---|
| Screen capture | ✅ MIT-SHM + DAMAGE (frames only on change) | ⏳ portal + PipeWire |
| Encoding | ✅ openh264 (software) | ✅ same |
| Hardware encoding | ⏳ VA-API | ⏳ VA-API |
| Pen with pressure | ✅ uinput (after `--setup-uinput`) | ✅ uinput (after `--setup-uinput`) |
| Two-finger scroll | ✅ uinput hi-res wheel | ✅ same |
| USB (tethering) | ✅ | ✅ |

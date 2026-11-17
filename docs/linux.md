# Inkbridge on Linux

Status: **X11 and Wayland sessions supported** (screen capture, H.264 with VA-API hardware encoding or
openh264 software fallback, pen with pressure via uinput, a no-setup pointer fallback on Wayland, USB
tethering). Wayland and VA-API are implemented and unit-tested; the VA-API encoder is verified on an
NVIDIA RTX 5060 through Mesa `d3d12` on WSL (the stream decodes with correct colours). End-to-end
validation on a real desktop (GNOME and KDE, Intel and AMD GPUs) is pending.

## Wayland: screen sharing permission

On Wayland, applications can't read the screen by themselves. At startup the host asks the desktop
through **xdg-desktop-portal**; GNOME, KDE and wlroots desktops show a dialog asking which monitor to
share. Inkbridge asks for persistent permission and stores the desktop's *restore token* in
`~/.config/inkbridge/` (`wayland-screencast-token` or `wayland-remote-desktop-token`), so the dialog
appears only the first time. You can revoke it in the desktop's privacy settings at any time.

Frames arrive over PipeWire only when the screen changes (the cursor is drawn into them).

## Pen input

### With pressure: the uinput option (one-time, optional)

Linux delivers real pen input (pressure, tilt, eraser, buttons) to applications such as Krita, GIMP,
Xournal++ and browsers from a *virtual tablet* created through `/dev/uinput`. That device node is root-only by
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

### Without setup (Wayland): pointer only

Without uinput access, a Wayland host also asks the portal for **remote control of the pointer**, in the
same dialog as screen sharing. The pen then works like a mouse: hover moves the pointer, touching holds
the left button, two-finger scrolling uses smooth scroll. It has **no pressure or tilt**: the portal has no
stylus events yet. libei's tablet interface will add them once compositors support it.

On X11 without uinput there is no pen input; run `--setup-uinput`.

## Hardware encoding (VA-API)

When the GPU's VA-API driver can encode H.264 (Intel `iHD`/`i965`, AMD `radeonsi`, Mesa `d3d12` on
WSL), the host uses it at the tablet's full resolution. Low-latency settings: I/P frames only, one
reference frame, CBR with about two frames of buffering, no periodic keyframes. Otherwise it falls back to
openh264 (software, resolution capped for speed). The host log says which one runs.

The encoder opens `/dev/dri/renderD*`; desktop sessions grant the logged-in user access automatically.
If the driver produces errors or empty output three times in a row, the host switches to openh264 for
the rest of the run.

### WSL (developers)

Mesa's `d3d12` driver forwards to the Windows GPU. It needs three things:

- membership in the `render` group: `sudo usermod -aG render,video $USER`, then `wsl --shutdown`;
- `GALLIUM_DRIVER=d3d12`, because WSL's render node belongs to `vgem` and Mesa doesn't pick `d3d12` for it
  on its own (otherwise: `d3d12_drv_video.so init failed`);
- on PCs with two GPUs, `MESA_D3D12_DEFAULT_ADAPTER_NAME=NVIDIA` (or `AMD`) to choose the discrete one.
  Mesa 26.0 on an Intel iGPU accepted encode jobs but produced no output; the host then falls back to
  software.

Hardware test: `GALLIUM_DRIVER=d3d12 MESA_D3D12_DEFAULT_ADAPTER_NAME=NVIDIA cargo test -p ib-encode
--features vaapi -- --ignored`.

## USB connection

Turn on **USB tethering** on the tablet and open Inkbridge there. The host finds the tethering interface
(kernel driver `rndis_host` or `cdc_ncm`), reads the tablet's address from the routing table (it is the
interface's gateway) and connects to it. Nothing needs configuring on Linux.

## Building

```sh
sudo apt install libpipewire-0.3-dev libspa-0.2-dev libva-dev libclang-dev pkg-config
cargo build --release -p ib-host
```

Wayland (`wayland`) and VA-API (`vaapi`) are default features of `ib-host`. Use
`cargo build -p ib-host --no-default-features` for an X11-only, software-encoding build that needs no
system packages. The libva bindings are generated from the installed headers at build time; libva keeps
its ABI stable, so the binary also runs with newer libva.

## Status table

| Feature | X11 | Wayland |
|---|---|---|
| Screen capture | ✅ MIT-SHM + DAMAGE (frames only on change) | ✅ portal + PipeWire (frames only on change), restore token |
| Hardware encoding | ✅ VA-API (CPU upload) | ✅ VA-API (CPU upload; DMA-BUF zero-copy later) |
| Software encoding | ✅ openh264 fallback | ✅ same |
| Pen with pressure | ✅ uinput (after `--setup-uinput`) | ✅ uinput (after `--setup-uinput`) |
| Pen without setup | — | ✅ pointer only (RemoteDesktop portal) |
| Two-finger scroll | ✅ uinput hi-res wheel | ✅ uinput, or portal smooth scroll |
| USB (tethering) | ✅ | ✅ |

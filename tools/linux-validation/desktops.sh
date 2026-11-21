#!/usr/bin/env bash
# Adds more desktops to an installed Ubuntu 26.04 test system, so the Linux host can be checked on
# each one: pick the session on the login screen (bottom-left/top-right menu).
#
#   GNOME (Wayland)   comes with Ubuntu: portal screen sharing + RemoteDesktop, mutter
#   KDE Plasma        Wayland: xdg-desktop-portal-kde, KWin (a different portal and compositor)
#   Xfce              X11: the XShm capture and XTest/uinput paths (GNOME and Plasma are Wayland-only)
#   Sway (--sway)     wlroots: xdg-desktop-portal-wlr shares the screen but has no RemoteDesktop,
#                     so only the uinput pen works there
#
# GDM no longer starts X11 sessions, so SDDM (Plasma's login screen) becomes the display manager;
# it starts all of them, GNOME included.
set -euo pipefail

sudo add-apt-repository -y universe >/dev/null
sudo apt-get update -qq
echo "sddm shared/default-x-display-manager select sddm" | sudo debconf-set-selections
sudo DEBIAN_FRONTEND=noninteractive apt-get install -y \
    kde-plasma-desktop sddm xdg-desktop-portal-kde plasma-workspace-wayland \
    xfce4 xfce4-terminal xdg-desktop-portal-gtk \
    xournalpp firefox
if [[ "${1:-}" == --sway ]]; then
    sudo apt-get install -y sway xdg-desktop-portal-wlr foot
fi
sudo dpkg-reconfigure -f noninteractive sddm
echo
echo "Done. Log out (or reboot) and choose the session on the login screen:"
echo "  Plasma (Wayland), GNOME, Xfce Session$([[ "${1:-}" == --sway ]] && echo ', Sway')"
echo "Check which one runs with:  echo \$XDG_SESSION_TYPE \$XDG_CURRENT_DESKTOP"

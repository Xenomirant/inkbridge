#!/usr/bin/env bash
# Connects a live session to an AnyConnect VPN with openconnect.
# Settings come from vpn.conf next to this script (VPN_SERVER, VPN_GROUP,
# VPN_USER; never a password) or are asked for. The password is typed in.
#   bash vpn.sh          connect (runs in the background)
#   bash vpn.sh stop     disconnect
set -u
KIT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PIDFILE=/run/inkbridge-vpn.pid

if [[ "${1:-}" == stop ]]; then
    sudo kill -INT "$(cat "$PIDFILE" 2>/dev/null)" 2>/dev/null && echo "VPN disconnected." || echo "VPN not running."
    exit 0
fi

VPN_SERVER="" VPN_GROUP="" VPN_USER=""
# shellcheck source=/dev/null
[[ -f "$KIT/vpn.conf" ]] && . "$KIT/vpn.conf"
[[ -z "$VPN_SERVER" ]] && read -r -p "VPN server: " VPN_SERVER
[[ -z "$VPN_USER" ]] && read -r -p "VPN user name: " VPN_USER

if ! command -v openconnect >/dev/null; then
    echo "Installing openconnect (needs the normal internet connection)…"
    sudo add-apt-repository -y universe >/dev/null 2>&1
    sudo apt-get update -qq && sudo apt-get install -y openconnect || exit 1
fi

args=(--protocol=anyconnect --user="$VPN_USER" --background --pid-file="$PIDFILE")
[[ -n "$VPN_GROUP" ]] && args+=(--authgroup="$VPN_GROUP")
echo "Connecting to $VPN_SERVER as $VPN_USER; type your VPN password when asked."
sudo openconnect "${args[@]}" "$VPN_SERVER" || exit 1
sleep 3
# Only Claude's API goes through the tunnel; crates.io, rustup, GitHub and apt work directly (and
# faster) on the campus network. Skip with VPN_FULL=1.
if [[ -z "${VPN_FULL:-}" ]]; then
    sudo ip route del default dev tun0 2>/dev/null
    sudo ip route replace 160.79.104.0/23 dev tun0
fi
code="$(curl -s -o /dev/null -w '%{http_code}' --max-time 10 https://api.anthropic.com/)"
if [[ "$code" != 403 && "$code" != 000 ]]; then
    echo "VPN up: api.anthropic.com is reachable."
else
    echo "VPN started, but api.anthropic.com still answers 403; check the connection."
fi

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
    sudo ip -6 route del unreachable 2607:6bc0::/48 2>/dev/null
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
# The tunnel's DNS servers (set by openconnect: systemd-resolved, else resolv.conf).
vpn_dns() {
    local ips
    ips="$(resolvectl dns tun0 2>/dev/null | sed 's/^[^:]*://')"
    # Without systemd-resolved, openconnect writes its servers into resolv.conf.
    [[ -z "${ips// /}" ]] && ips="$(awk '/^nameserver/ {print $2}' /etc/resolv.conf 2>/dev/null)"
    tr ' ' '\n' <<<"$ips" | grep -E '^[0-9]+(\.[0-9]+){3}$' | grep -v '^127\.' | sort -u
}
api_ok() {
    getent ahostsv4 api.anthropic.com >/dev/null || return 1
    local code
    code="$(curl -4 -s -o /dev/null -w '%{http_code}' --max-time 10 https://api.anthropic.com/)"
    [[ "$code" != 403 && "$code" != 000 ]]
}
# Only Claude's API goes through the tunnel; crates.io, rustup, GitHub and apt work directly (and
# faster) on the campus network. The tunnel's DNS servers go through it too: they answer only
# there. Anthropic's IPv6 range is made unreachable so programs fall back to IPv4 at once instead
# of trying it outside the tunnel. VPN_FULL=1 keeps everything in the tunnel.
if [[ -z "${VPN_FULL:-}" ]]; then
    dns="$(vpn_dns)"
    sudo ip route del default dev tun0 2>/dev/null
    sudo ip route replace 160.79.104.0/23 dev tun0
    for d in $dns; do sudo ip route replace "$d/32" dev tun0; done
    sudo ip -6 route replace unreachable 2607:6bc0::/48 2>/dev/null
    sudo resolvectl flush-caches 2>/dev/null
    if ! api_ok; then
        echo "Claude-only routing didn't work here (DNS: ${dns:-none found}); sending everything through the VPN."
        sudo ip route replace default dev tun0
    fi
fi
if api_ok; then
    echo "VPN up: api.anthropic.com is reachable."
else
    echo "VPN started, but api.anthropic.com isn't reachable. Details:"
    echo "  DNS: $(getent ahostsv4 api.anthropic.com | head -1 || echo 'no answer')"
    echo "  route: $(ip route get 160.79.104.10 2>&1 | head -1)"
    echo "  tunnel DNS servers: $(vpn_dns | tr '\n' ' ')"
fi

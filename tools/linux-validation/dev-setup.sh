#!/usr/bin/env bash
# Optional: turns the live session into a development machine, so fixes can
# be made and tested on the spot. Run after `bash vpn.sh` if you need it.
#   - Claude Code: the binary in this kit if present, else the official installer
#   - build packages + Rust (rustup, user-level)
#   - the source: inkbridge.bundle from this kit → ~/inkbridge
set -u
KIT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

mkdir -p "$HOME/.local/bin"
if [[ -f "$KIT/claude" ]]; then
    install -m 755 "$KIT/claude" "$HOME/.local/bin/claude"
else
    curl -fsSL https://claude.ai/install.sh | bash
fi
grep -q '.local/bin' "$HOME/.bashrc" || echo 'export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$PATH"' >>"$HOME/.bashrc"
export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$PATH"

sudo add-apt-repository -y universe >/dev/null 2>&1
sudo apt-get update -qq
sudo apt-get install -y build-essential pkg-config git curl clang libclang-dev \
    libpipewire-0.3-dev libspa-0.2-dev libva-dev libxcb1-dev libxcb-shm0-dev libxcb-randr0-dev \
    libudev-dev intel-media-va-driver vainfo

command -v cargo >/dev/null || curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal -c clippy,rustfmt

if [[ -f "$KIT/inkbridge.bundle" && ! -d "$HOME/inkbridge" ]]; then
    git clone -q "$KIT/inkbridge.bundle" "$HOME/inkbridge"
fi
[[ -f "$KIT/HANDOFF.md" && -d "$HOME/inkbridge" ]] && cp "$KIT/HANDOFF.md" "$HOME/inkbridge/"

echo
echo "Ready. Open a new terminal, then:"
echo "  cd ~/inkbridge && claude      (log in when asked; read HANDOFF.md first)"
echo "Remember: the live session forgets everything at shutdown. Push or copy work out before."

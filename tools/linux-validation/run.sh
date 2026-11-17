#!/usr/bin/env bash
# Inkbridge Linux validation, for a live-USB Ubuntu session (or any Linux
# desktop). Run from the kit folder:   bash run.sh
#
# Steps (each can be skipped): self-test → tablet in pointer mode →
# tablet with pressure (uinput). Results land in results-<date>/ next to this
# script when the stick is writable, else in ~/inkbridge-results.
set -u

KIT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
STAMP="$(date +%Y%m%d-%H%M%S)"
WORK="$HOME/inkbridge-kit"          # binaries run from here (the stick may be noexec)
BIN="$WORK/inkbridge-host"

say()  { printf '\n\033[1m%s\033[0m\n' "$*"; }
ask()  { local a; read -r -p ">>> $* [y/n] " a; [[ "$a" == [yY]* ]]; }
note() { printf '%s\n' "$*" >>"$OUT/answers.txt"; }
answer() { # question → recorded yes/no
    if ask "$1"; then note "yes: $1"; else note "NO:  $1"; fi
}

# --- where results go -------------------------------------------------------
pick_results_dir() {
    local dir="$KIT/results-$STAMP"
    if mkdir -p "$dir" 2>/dev/null && touch "$dir/.w" 2>/dev/null; then
        rm -f "$dir/.w"; echo "$dir"; return
    fi
    # Rufus-made sticks are FAT32/NTFS but mounted read-only at /cdrom.
    local mnt
    mnt="$(findmnt -n -o TARGET --target "$KIT" 2>/dev/null)"
    if [[ -n "$mnt" ]] && sudo mount -o remount,rw "$mnt" 2>/dev/null \
        && mkdir -p "$dir" 2>/dev/null; then
        echo "$dir"; return
    fi
    mkdir -p "$HOME/inkbridge-results-$STAMP"; echo "$HOME/inkbridge-results-$STAMP"
}
OUT="$(pick_results_dir)"
say "Results will be saved to: $OUT"

mkdir -p "$WORK"
cp "$KIT/inkbridge-host" "$BIN" && chmod +x "$BIN"
cp "$KIT/pen-test.html" "$WORK/" 2>/dev/null

# --- system facts ------------------------------------------------------------
{
    echo "== date";        date -R
    echo "== os";          grep PRETTY_NAME /etc/os-release; uname -r
    echo "== session";     echo "type=${XDG_SESSION_TYPE:-} desktop=${XDG_CURRENT_DESKTOP:-}"
    echo "== gpus";        lspci -nnk 2>/dev/null | grep -A3 -Ei 'vga|3d|display'
    echo "== va drivers";  ls /usr/lib/x86_64-linux-gnu/dri/ 2>/dev/null | grep -i drv_video
    echo "== packages";    dpkg -l 2>/dev/null | grep -Ei 'libva|intel-media|mesa-va|pipewire |xdg-desktop-portal' | awk '{print $2, $3}'
    echo "== libraries";   ldd "$BIN"
    echo "== network";     ip -brief addr
} >"$OUT/system.txt" 2>&1

# --- missing runtime libraries / VA driver -----------------------------------
if ldd "$BIN" | grep -q 'not found'; then
    say "Some libraries the host needs are missing:"
    ldd "$BIN" | grep 'not found'
    if ask "Install them now (needs internet)?"; then
        sudo apt-get update && sudo apt-get install -y libva2 libva-drm2 libpipewire-0.3-0t64
    fi
fi
if lspci -n 2>/dev/null | grep -q ' 0300: 8086:\| 0380: 8086:' \
    && ! ls /usr/lib/x86_64-linux-gnu/dri/iHD_drv_video.so >/dev/null 2>&1; then
    say "The Intel GPU's video encoder driver (intel-media-va-driver) isn't installed."
    if ask "Install it now (needs internet; enables hardware encoding)?"; then
        sudo add-apt-repository -y universe >/dev/null 2>&1
        sudo apt-get update && sudo apt-get install -y intel-media-va-driver
        dpkg -l intel-media-va-driver | tail -1 >>"$OUT/system.txt"
    fi
fi

# --- 1. self-test ---------------------------------------------------------------
say "Step 1/3: self-test (encoder, screen sharing, pointer). About 1 minute."
if ask "Start the self-test?"; then
    RUST_LOG=info "$BIN" --self-test "$OUT/selftest.txt" 2>"$OUT/selftest-log.txt"
    say "Self-test finished (details in selftest.txt)."
fi

host_session() { # $1 = log name
    say "Starting the host. A pairing QR code opens in the browser."
    cat <<'EOF'
On the tablet:
  1. Connect it to this PC with the USB cable.
  2. Settings → Network → Hotspot & tethering → turn on USB tethering.
  3. Open Inkbridge and scan the QR code (or pick this PC from the list).
Then try what the questions below ask about.
Press Ctrl+C here when you're done.
EOF
    RUST_LOG=info,ib_host=debug "$BIN" --pair 2>&1 | tee "$OUT/$1"
}

# --- 2. tablet, pointer mode ----------------------------------------------------
say "Step 2/3: the tablet as a screen with the pen as a mouse (no setup)."
if ask "Run this step?"; then
    note "== step 2: pointer mode"
    host_session host-pointer.log
    answer "Did the PC screen appear on the tablet?"
    answer "Did the picture look right (colours, sharpness, nothing shifted)?"
    answer "Did hovering the pen move the mouse pointer to the right place?"
    answer "Did touching the pen click/drag (e.g. select text, move a window)?"
    answer "Did two-finger scrolling work?"
    answer "Did the picture follow your actions quickly (no obvious lag)?"
    read -r -p ">>> Anything else you noticed (Enter to skip): " extra; [[ -n "$extra" ]] && note "note: $extra"
fi

# --- 3. tablet with pressure (uinput) ------------------------------------------
say "Step 3/3: pen pressure. Uses the Advanced option (a udev rule; a live session forgets it on reboot)."
if ask "Run this step?"; then
    note "== step 3: pressure (uinput)"
    # The real user path first (the desktop's admin prompt), sudo if that fails.
    if ! "$BIN" --setup-uinput >"$OUT/setup-uinput.log" 2>&1; then
        echo "(admin prompt failed; retrying with sudo)" >>"$OUT/setup-uinput.log"
        sudo "$BIN" --setup-uinput >>"$OUT/setup-uinput.log" 2>&1
    fi
    sleep 1
    { ls -l /dev/uinput; getfacl /dev/uinput 2>/dev/null; } >>"$OUT/setup-uinput.log" 2>&1
    cat "$OUT/setup-uinput.log"
    if ask "Install Xournal++ to draw with pressure (needs internet)?"; then
        sudo add-apt-repository -y universe >/dev/null 2>&1
        sudo apt-get update && sudo apt-get install -y xournalpp && (xournalpp >/dev/null 2>&1 &)
    fi
    echo "Also open $WORK/pen-test.html in Firefox: it shows pressure and tilt values."
    host_session host-pressure.log
    answer "Did strokes get thicker/darker when pressing harder (Xournal++ or the test page)?"
    answer "Did the test page show tilt values changing?"
    answer "Did hover (pen above the screen) move the pointer without drawing?"
    answer "Did the eraser end or the pen button work (if your pen has them)?"
    read -r -p ">>> Anything else you noticed (Enter to skip): " extra; [[ -n "$extra" ]] && note "note: $extra"
fi

sync
say "Done. Everything is in: $OUT"
if [[ "$OUT" == "$HOME"/* ]]; then
    echo "!!! The USB stick wasn't writable, so the results are in the live session's memory."
    echo "!!! Copy that folder to another drive (Files app) before shutting down, or it's lost."
fi
echo "Shut down, boot Windows, and tell Claude where the results are."

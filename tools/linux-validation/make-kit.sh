#!/usr/bin/env bash
# Builds the Linux validation kit into target/linux-kit/inkbridge/:
# copy that folder to the root of a live-USB stick and run `bash run.sh`
# from it in the live session. Build on the same (or older) Ubuntu release
# as the live system, so the binary's glibc matches.
#
#   make-kit.sh --with-dev   also adds the source (git bundle) and this
#                            machine's Claude Code binary, for dev-setup.sh
# Files in tools/linux-validation/personal/ (git-ignored: e.g. vpn.conf,
# HANDOFF.md) are copied into the kit as well. Never put passwords there.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
OUT="$ROOT/target/linux-kit/inkbridge"

cargo build --release -p ib-host --manifest-path "$ROOT/Cargo.toml"
rm -rf "$OUT" && mkdir -p "$OUT"
cp "$ROOT/target/release/inkbridge-host" "$OUT/"
strip "$OUT/inkbridge-host"
for f in run.sh vpn.sh dev-setup.sh README.txt; do cp "$ROOT/tools/linux-validation/$f" "$OUT/"; done
[[ -d "$ROOT/tools/linux-validation/personal" ]] && cp -r "$ROOT/tools/linux-validation/personal/." "$OUT/"
if [[ "${1:-}" == --with-dev ]]; then
    git -C "$ROOT" bundle create "$OUT/inkbridge.bundle" --all
    claude_bin="$(readlink -f "$(command -v claude)")" && cp "$claude_bin" "$OUT/claude"
fi
cp "$ROOT/tools/pen-test/index.html" "$OUT/pen-test.html"
(cd "$OUT" && sha256sum inkbridge-host >SHA256SUMS)
echo "Kit ready: $OUT ($(du -sh "$OUT" | cut -f1))"

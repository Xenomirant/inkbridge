#!/usr/bin/env bash
# Builds the Linux validation kit into target/linux-kit/inkbridge/:
# copy that folder to the root of a live-USB stick and run `bash run.sh`
# from it in the live session. Build on the same (or older) Ubuntu release
# as the live system, so the binary's glibc matches.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
OUT="$ROOT/target/linux-kit/inkbridge"

cargo build --release -p ib-host --manifest-path "$ROOT/Cargo.toml"
rm -rf "$OUT" && mkdir -p "$OUT"
cp "$ROOT/target/release/inkbridge-host" "$OUT/"
strip "$OUT/inkbridge-host"
cp "$ROOT/tools/linux-validation/run.sh" "$ROOT/tools/linux-validation/README.txt" "$OUT/"
cp "$ROOT/tools/pen-test/index.html" "$OUT/pen-test.html"
(cd "$OUT" && sha256sum inkbridge-host >SHA256SUMS)
echo "Kit ready: $OUT ($(du -sh "$OUT" | cut -f1))"

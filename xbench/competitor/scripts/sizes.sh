#!/usr/bin/env bash
# Print binary + per-crate stripped sizes for the SPG distribution.
#
# Used as input to PERFORMANCE.md's "Footprint" section. Run from
# anywhere in the workspace.

set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

# Build everything in release so the numbers below reflect what
# ships, not the debug bloat. -q hides cargo's progress noise so the
# output is a clean table.
# `grep -v` returns exit 1 when nothing matches (cargo -q succeeded
# with empty stderr), which `set -e` would treat as fatal — append
# `|| true` so a clean build doesn't kill the script.
cargo build --release --workspace -q 2>&1 \
    | grep -vE '^(    Compiling|   Compiling|     Finished|warning)' \
    || true

# `cargo metadata` is the only reliable way to find the workspace's
# target dir — CARGO_TARGET_DIR may be set by an external wrapper
# (cargo-target-dir.md) but isn't visible to shell scripts that don't
# spawn cargo first.
metadata_target=$(cargo metadata --no-deps --format-version 1 2>/dev/null \
    | python3 -c "import json,sys; print(json.load(sys.stdin).get('target_directory',''))")
target_dir="${metadata_target:-target}/release"

echo
echo "== final binaries (stripped, release) =="
# spg-cli's binary is named `spg` (per its Cargo.toml [[bin]] name).
for bin in spg-server spg; do
    if [ -f "$target_dir/$bin" ]; then
        size_h=$(du -h "$target_dir/$bin" | cut -f1)
        size_b=$(stat -f%z "$target_dir/$bin" 2>/dev/null || stat -c%s "$target_dir/$bin")
        printf "  %-15s %8s  (%d bytes)\n" "$bin" "$size_h" "$size_b"
    fi
done

echo
echo "== per-crate rlib sizes (deps dir) =="
deps="$target_dir/deps"
# rlib names follow `libspg_*-<hash>.rlib`. Take the largest per
# crate (oldest hash variants can coexist).
for crate in spg_wire spg_sql spg_storage spg_crypto spg_audit spg_engine; do
    line=$(ls -S "$deps"/lib${crate}-*.rlib 2>/dev/null | head -1)
    if [ -n "$line" ]; then
        size_h=$(du -h "$line" | cut -f1)
        printf "  %-15s %8s\n" "$crate" "$size_h"
    fi
done

echo
echo "== libstd's contribution (sanity) =="
ls -lh "$deps"/libstd-*.rlib 2>/dev/null | head -1 | awk '{ printf "  %-15s %s\n", "libstd", $5 }'

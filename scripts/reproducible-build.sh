#!/usr/bin/env bash
# reproducible-build.sh — Builds the release binary with reproducibility flags (ADR-0015).
#
# Includes:
#   - SOURCE_DATE_EPOCH (HEAD commit timestamp) → avoids variable build timestamps
#   - CARGO_INCREMENTAL=0                        → no non-deterministic incremental caches
#   - --remap-path-prefix  $HOME, $CARGO_HOME   → no builder FS paths in the binary
#
# Does NOT build the bundle (.app/.dmg/.AppImage) — those are NOT bit-for-bit reproducible
# without upstream support in Tauri (Info.plist timestamps, code-sign, etc.).
#
# Usage:
#   ./scripts/reproducible-build.sh              # cargo clean + cargo build --release
#   ./scripts/reproducible-build.sh --no-clean   # skip cargo clean (faster, but second run
#                                                #   reuses cache → hashes match trivially,
#                                                #   NOT a real reproducibility proof)
#   ./scripts/reproducible-build.sh --bin plugin-hash
#
# B22: cargo clean is mandatory for a valid reproducibility test. Without it, two consecutive
# runs reuse the cache and produce identical hashes BY DEFINITION, not because the build is
# truly reproducible. Use --no-clean only for speed during development, never to claim
# reproducibility across separate environments.
#
# Manual verification:
#   ./scripts/reproducible-build.sh && HASH1=$(cat /tmp/nexe-build.hash)
#   ./scripts/reproducible-build.sh && HASH2=$(cat /tmp/nexe-build.hash)
#   [[ "$HASH1" == "$HASH2" ]] && echo "✅ reproducible" || echo "❌ differs"

set -euo pipefail

# B22: parse --no-clean flag
NO_CLEAN=0
PASSTHROUGH_ARGS=()
for arg in "$@"; do
    if [[ "$arg" == "--no-clean" ]]; then
        NO_CLEAN=1
    else
        PASSTHROUGH_ARGS+=("$arg")
    fi
done

cd "$(dirname "$0")/.."

# SOURCE_DATE_EPOCH: HEAD commit timestamp (fallback: now if there is no git).
SOURCE_DATE_EPOCH="$(git log -1 --format=%ct HEAD 2>/dev/null || date +%s)"
export SOURCE_DATE_EPOCH

# No incremental caches (they can introduce non-determinism between builds).
export CARGO_INCREMENTAL=0

# Remap absolute → placeholders (masked in panic traces and DWARF).
# Note: config.toml has --remap-path-prefix=@CARGO_HOME=@cargo with a literal token;
# here we inject the real value in case it is not on PATH. Additive with prior RUSTFLAGS.
CARGO_HOME_VAL="${CARGO_HOME:-$HOME/.cargo}"
export RUSTFLAGS="--remap-path-prefix=${HOME}=~ --remap-path-prefix=${CARGO_HOME_VAL}=@cargo ${RUSTFLAGS:-}"

echo "=== Reproducible build config ==="
echo "SOURCE_DATE_EPOCH=$SOURCE_DATE_EPOCH ($(date -u -r "$SOURCE_DATE_EPOCH" '+%Y-%m-%d %H:%M UTC' 2>/dev/null || echo 'epoch'))"
echo "CARGO_INCREMENTAL=$CARGO_INCREMENTAL"
echo "RUSTFLAGS=$RUSTFLAGS"
echo "HEAD=$(git rev-parse --short HEAD 2>/dev/null || echo 'no-git')"
echo ""

cd src-tauri

# B22: cargo clean before build to ensure cache does not mask non-reproducibility.
# Skip only when --no-clean is explicitly passed (development convenience only).
if [[ $NO_CLEAN -eq 0 ]]; then
    echo "=== cargo clean (B22: required for valid reproducibility test) ==="
    cargo clean
    echo ""
else
    echo "⚠️  WARNING: --no-clean specified — cache reuse means identical hashes prove nothing"
    echo "   Use two separate clean builds to verify reproducibility."
    echo ""
fi

cargo build --release --locked "${PASSTHROUGH_ARGS[@]+"${PASSTHROUGH_ARGS[@]}"}"

# Hash of the main binary (may differ with --bin)
BIN="target/release/nexe-app"
if [[ -f "$BIN" ]]; then
    if command -v shasum &>/dev/null; then
        HASH=$(shasum -a 256 "$BIN" | awk '{print $1}')
    else
        HASH=$(sha256sum "$BIN" | awk '{print $1}')
    fi
    SIZE=$(stat -f%z "$BIN" 2>/dev/null || stat -c%s "$BIN")
    echo ""
    echo "=== Build output ==="
    echo "Binary: src-tauri/$BIN"
    echo "Size:   $SIZE bytes"
    echo "SHA256: $HASH"
    echo "$HASH" > /tmp/nexe-build.hash
    echo ""
    echo "Hash saved to /tmp/nexe-build.hash (for reproducibility check)"
fi

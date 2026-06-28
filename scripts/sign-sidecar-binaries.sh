#!/usr/bin/env bash
# ────────────────────────────────────────────────────────────────────────
# sign-sidecar-binaries.sh
# Re-signs every Mach-O binary inside target/sidecar/ with our Developer
# ID certificate, secure timestamp, and hardened runtime.
#
# Initial scope: target/sidecar/venv (333 Mach-O wheels + libs).
# Later (2026-05-18): scope expanded to the full target/sidecar/ — includes
# python-runtime/ (PBS copied into the bundle): bin/python3.12 + ~45-55 .so
# from lib-dynload (ssl, socket, hashlib, ctypes, sqlite3, ...). codesign
# --force replaces the upstream CMS signature of the PBS with our
# Developer ID — correct behaviour for Apple notarytool, which requires
# all the Mach-O in the bundle to be signed with the same identity
# as the app. Without this expansion, the PBS python3.12 would be left
# with the upstream CMS signature that Gatekeeper accepts but notarytool does
# not validate as part of our Developer ID team.
#
# Why: the PBS venv contains hundreds of .so/.dylib signed ad-hoc
# by the wheel authors (or unsigned). Apple notarization rejects the whole
# .app if any nested Mach-O lacks Developer ID + timestamp +
# hardened runtime. A failed submission listed _miniaudio.abi3.so,
# _cffi_backend, mmh3, and dozens more nested Mach-O files as issues.
#
# Difference vs server-nexe legacy `sign-wheels-bundle.sh`: that one operated
# on .whl files (unpack → sign → repack with sha256 RECORD). Here the
# venv is already extracted (PBS+uv install), we just need to walk the bundle and
# sign in-place — no zip round-trip.
#
# Usage:
#   APPLE_SIGNING_IDENTITY="Developer ID Application: Your Name (TEAMID)" \
#     bash scripts/sign-sidecar-binaries.sh [SIDECAR_DIR]
#
# If APPLE_SIGNING_IDENTITY is unset, the script exits 0 with a warning
# (dev-mode build without certificate). Release builds must set it.
# ────────────────────────────────────────────────────────────────────────
set -euo pipefail

SIDECAR_DIR="${1:-$(cd "$(dirname "$0")/.." && pwd)/target/sidecar}"

if [ ! -d "$SIDECAR_DIR/venv" ]; then
    echo "ERROR: venv directory missing: $SIDECAR_DIR/venv" >&2
    echo "       Run scripts/build-sidecar.sh first." >&2
    exit 1
fi

if [ -z "${APPLE_SIGNING_IDENTITY:-}" ]; then
    echo "==> sign-sidecar-binaries: APPLE_SIGNING_IDENTITY unset → skipping (dev build)"
    exit 0
fi

# Verify the identity is actually available in the keychain. `find-identity`
# matches by substring so we strip the cert prefix and grep for the unique
# Team ID in parens.
if ! security find-identity -v -p codesigning 2>/dev/null | grep -qF "$APPLE_SIGNING_IDENTITY"; then
    echo "ERROR: Signing identity not found in keychain: $APPLE_SIGNING_IDENTITY" >&2
    echo "       Available identities:" >&2
    security find-identity -v -p codesigning >&2
    exit 2
fi

echo "==> Signing Mach-O binaries in $SIDECAR_DIR"
echo "    Scope: venv/ + python-runtime/"
echo "    Identity: $APPLE_SIGNING_IDENTITY"

TOTAL=0
SIGNED=0
SKIPPED=0
FAILED=0
START=$(date +%s)

# Candidate files: .so, .dylib, anything with exec bit (catches torch's
# protoc, torch_shm_manager, PBS python3.12 binary, etc.).
# Filter actual Mach-O via `file` magic (skips shell scripts, .pyc, text).
# find now covers the full $SIDECAR_DIR — venv/ + python-runtime/
# (PBS). See the header for justification.
while IFS= read -r -d '' f; do
    if ! file "$f" 2>/dev/null | grep -q "Mach-O"; then
        SKIPPED=$((SKIPPED + 1))
        continue
    fi
    TOTAL=$((TOTAL + 1))
    # B140: a binary that signs (exit 0) but fails --verify --strict (inconsistent
    # metadata) would otherwise pass as SIGNED here and only blow up at notarytool.
    # Fold the post-signature verify into the same gate so it counts as FAILED and
    # trips the existing abort below. Per-file (not sampling) is correct; ~2× codesign
    # calls on a script that already takes 1-3 min. No spctl on flat dylibs (wrong tool).
    if codesign --force --options=runtime --timestamp \
            --sign "$APPLE_SIGNING_IDENTITY" "$f" >/dev/null 2>&1 \
        && codesign --verify --strict "$f" >/dev/null 2>&1; then
        SIGNED=$((SIGNED + 1))
    else
        FAILED=$((FAILED + 1))
        echo "    sign/verify failed: ${f#"$SIDECAR_DIR/"}" >&2
    fi
done < <(find "$SIDECAR_DIR" -type f \
    \( -name "*.so" -o -name "*.dylib" -o -perm +111 \) \
    -print0)

ELAPSED=$(($(date +%s) - START))

echo "==> Sign report"
echo "    Mach-O signed:    $SIGNED / $TOTAL"
echo "    Non-Mach-O skipped: $SKIPPED"
echo "    Failed:           $FAILED"
echo "    Time:             ${ELAPSED}s"

if [ "$FAILED" -gt 0 ]; then
    echo "❌ $FAILED Mach-O failed to sign — aborting" >&2
    exit 3
fi

echo "✓ All Mach-O signed with Developer ID + timestamp + hardened runtime"

#!/usr/bin/env bash
# pre-bundle-sidecar.sh — create sidecar-bundle.tar.gz for Tauri resource bundling.
#
# Why tarball instead of a directory glob: Tauri 2 glob '**' does not recurse into
# deep directories, so the PBS+uv venv cannot be included via tauri.conf.json directly.
# A single .tar.gz sidesteps the bundler limitation entirely.
#
# Initially: only venv/ + app/ archived.
# Later (2026-05-18): venv/ + app/ + python-runtime/ — the PBS copied by
# build-sidecar.sh Step 5.5 must travel in the tarball so the venv's relative
# symlinks (../../python-runtime/bin/python3.12) can resolve on the
# target Mac. Without this, the sidecar never boots outside the build Mac.
#
# Only venv/ + app/ + python-runtime/ are archived — the nexe-sidecar launcher
# is managed by externalBin (Contents/MacOS/) and does not belong in the tarball.
#
# At first launch, Rust extracts the tarball to app_data_dir/sidecar/ and sets
# NEXE_SIDECAR_DIR so the launcher finds venv/ and app/ there. A version-stamped
# .extracted marker prevents re-extraction unless the app version changes.
#
# Prerequisites: run scripts/build-sidecar.sh first to generate target/sidecar/.
# Usage: called automatically by `pnpm tauri:build` via tauri.conf.json beforeBundleCommand.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
SIDECAR_SRC="$ROOT/target/sidecar"
TARBALL="$ROOT/src-tauri/sidecar-bundle.tar.gz"

if [ ! -d "$SIDECAR_SRC/venv" ]; then
  echo "pre-bundle-sidecar: target/sidecar/venv/ not found — run scripts/build-sidecar.sh first"
  exit 1
fi
# python-runtime/ must exist (build-sidecar.sh Step 5.5).
if [ ! -d "$SIDECAR_SRC/python-runtime" ]; then
  echo "pre-bundle-sidecar: ERROR — target/sidecar/python-runtime/ not found." >&2
  echo "                    Run scripts/build-sidecar.sh (Step 5.5)." >&2
  exit 2
fi

# B183/B184: defense-in-depth — the privacy gate lives in build-sidecar.sh,
# but pre-bundle can be called on its own (Tauri's beforeBundleCommand) over a
# potentially dirty staging → we re-verify that app/ does not drag in DEV/test data before
# sealing the distributed tarball.
if [ -d "$SIDECAR_SRC/app" ]; then
    "$SCRIPT_DIR/verify-privacy-gate.sh" "$SIDECAR_SRC/app" || exit 1
fi

echo "pre-bundle-sidecar: creating sidecar-bundle.tar.gz..."
rm -f "$TARBALL"
# Strip macOS AppleDouble (._*) files BEFORE tarball creation. Without
# this, the venv site-packages carry ~6400 ._*.py metadata files that the
# `transformers` library treats as Python source when scanning models/ and the
# scan crashes with UnicodeDecodeError (byte 0xa3 of the AppleDouble header
# magic is not valid UTF-8). COPYFILE_DISABLE=1 + --no-mac-metadata tell macOS
# tar to not emit fresh ones during archival. The `find -delete` deals with
# any already-present hidden metadata in the source tree.
find "$SIDECAR_SRC" -name '._*' -delete 2>/dev/null || true
# --no-same-owner --no-acls --no-xattrs strip build-machine ownership
# and extended attributes that would drag the build machine's UID/GID + quarantine
# flags into the final bundle.
#
# Linux portability: GNU tar does NOT accept `--no-mac-metadata`
# (it is a BSD tar / macOS exclusive flag) nor `--no-acls --no-xattrs` (different
# syntax). We detect the real tar and branch:
#   - GNU tar (Linux): omit BSD flags; the prior `find -delete` already cleans AppleDouble.
#   - BSD tar (macOS): keep the original behaviour + historical fallback.
if tar --version 2>&1 | grep -q "GNU tar"; then
    # Linux / GNU tar — no BSD flags. --owner=0 --group=0 normalizes UID/GID
    # (functional equivalent of --no-same-owner, but applied at creation).
    tar --owner=0 --group=0 -czf "$TARBALL" \
        -C "$SIDECAR_SRC" venv app python-runtime
else
    # macOS / BSD tar — original behaviour. The fallback (||) covers the
    # case where a future macOS tar changes the syntax of some optional flag.
    COPYFILE_DISABLE=1 tar --no-mac-metadata --no-same-owner --no-acls --no-xattrs \
        -czf "$TARBALL" -C "$SIDECAR_SRC" venv app python-runtime 2>/dev/null \
        || COPYFILE_DISABLE=1 tar --no-same-owner -czf "$TARBALL" \
            -C "$SIDECAR_SRC" venv app python-runtime
fi
echo "pre-bundle-sidecar: done ($(du -sh "$TARBALL" | cut -f1))"

# Generate SHA-256 of the tarball so the runtime can detect re-builds within
# the same CARGO_PKG_VERSION. Without this, `.extracted` marker compares
# version text and dev re-builds of the same version never re-extract the
# sidecar — see memory `feedback_extracted_marker_version_no_hash.md`.
SHA_FILE="${TARBALL%.tar.gz}.sha256"
SHASUM=$(shasum -a 256 "$TARBALL" 2>/dev/null | awk '{print $1}' \
        || sha256sum "$TARBALL" 2>/dev/null | awk '{print $1}')
if [ -n "$SHASUM" ]; then
    printf '%s  %s\n' "$SHASUM" "${TARBALL##*/}" > "$SHA_FILE"
    echo "pre-bundle-sidecar: sha256 = $SHASUM"
else
    echo "pre-bundle-sidecar: WARNING — neither shasum nor sha256sum found; SHA file not written" >&2
    : > "$SHA_FILE"  # empty placeholder → runtime falls back to version
fi

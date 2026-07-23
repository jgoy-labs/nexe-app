#!/usr/bin/env bash
# sign-macos.sh — macOS signing + notarization for public nexe-app releases.
#
# Full flow (ADR-0008 active):
#   1. cargo tauri build --release  → generates unsigned .app + .dmg
#   2. codesign applied via TAURI_SIGNING_IDENTITY (app signing)
#   3. xcrun notarytool submit    → Apple notarization servers
#   4. xcrun stapler staple       → staples the notarization ticket to the DMG
#
# Prerequisites (configure once before the first signed release):
#   - Apple Developer Program account (99 USD/year)
#   - Developer ID Application certificate in the Keychain
#   - Apple ID app-specific password (for notarytool)
#
# Usage:
#   export TAURI_SIGNING_IDENTITY="Developer ID Application: Your Name (TEAMID)"
#   export TAURI_APPLE_ID="your-apple-id@example.com"
#   export TAURI_APPLE_PASSWORD="@keychain:AC_PASSWORD"  # or direct value
#   export TAURI_APPLE_TEAM_ID="XXXXXXXXXX"
#   ./scripts/sign-macos.sh
#
# Verification is STRICT by default (WSF-001): spctl/notarization failures
# abort the build. For pre-notarization inspection runs (no ticket yet):
#   NEXE_STRICT_SIGNING=0 ./scripts/sign-macos.sh
#
# Troubleshooting:
#   - `security find-identity -v -p codesigning` → view installed certificates
#   - `xcrun notarytool history --apple-id ... --team-id ...` → review submissions

set -euo pipefail
cd "$(dirname "$0")/.."

# Environment variable validation
: "${TAURI_SIGNING_IDENTITY:?Need TAURI_SIGNING_IDENTITY (ex: 'Developer ID Application: Name (TEAMID)')}"
: "${TAURI_APPLE_ID:?Need TAURI_APPLE_ID (Apple ID email)}"
: "${TAURI_APPLE_PASSWORD:?Need TAURI_APPLE_PASSWORD (use @keychain:NAME, never a literal password)}"
: "${TAURI_APPLE_TEAM_ID:?Need TAURI_APPLE_TEAM_ID (10 char team ID)}"

# B21: reject literal passwords — must use @keychain: reference to avoid
# exposing credentials in shell history, CI logs, or process table.
if [ -n "${TAURI_APPLE_PASSWORD:-}" ] && [[ "$TAURI_APPLE_PASSWORD" != @keychain:* ]]; then
    echo "ERROR: TAURI_APPLE_PASSWORD must use @keychain: prefix (never a literal password)" >&2
    echo "       Set it with: security add-generic-password -a your@apple.id -s AC_PASSWORD -w" >&2
    echo "       Then export TAURI_APPLE_PASSWORD=@keychain:AC_PASSWORD" >&2
    exit 1
fi

echo "=== macOS Signing + Notarization ==="
echo "Identity : $TAURI_SIGNING_IDENTITY"
echo "Apple ID : $TAURI_APPLE_ID"
echo "Team ID  : $TAURI_APPLE_TEAM_ID"
echo ""

# Variables that the tauri bundler reads directly
export APPLE_SIGNING_IDENTITY="$TAURI_SIGNING_IDENTITY"
export APPLE_ID="$TAURI_APPLE_ID"
export APPLE_PASSWORD="$TAURI_APPLE_PASSWORD"
export APPLE_TEAM_ID="$TAURI_APPLE_TEAM_ID"

# B141: build hygiene for the public DMG — the `cargo tauri build` below builds
# the distributed binary, so we apply the same path remap + SOURCE_DATE_EPOCH
# as scripts/reproducible-build.sh to avoid embedding builder paths ($HOME,
# $CARGO_HOME) in the panic traces / DWARF of the binary that reaches the user (P4 leak).
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

# WSF-002: fail-closed vulnerability gate before building the DISTRIBUTED binary.
# The public DMG is built HERE (locally, not in CI), so a red `cargo audit` must block
# the release exactly as it blocks the CI pipeline (.github/workflows/{check,release}.yml).
# Runs in a subshell so it does not disturb the `cd src-tauri` for the build below.
echo "=== cargo audit (release gate) ==="
if ! command -v cargo-audit >/dev/null 2>&1; then
    echo "❌  cargo-audit not installed — required to gate the public release." >&2
    echo "    Install with: cargo install cargo-audit --locked" >&2
    exit 1
fi
( cd src-tauri && cargo audit --deny warnings )
echo ""

echo "=== cargo tauri build --release ==="
cd src-tauri
cargo tauri build

echo ""
echo "=== Verificacio signatura ==="
APP="target/release/bundle/macos/nexe-app.app"
if [[ -d "$APP" ]]; then
    codesign -dvv "$APP" 2>&1 | grep -E "Identifier|TeamIdentifier|Authority|Sealed"
    # WSF-001: STRICT per DEFECTE — aquest script només existeix per a
    # releases públiques (header), així que una fallada d'spctl ha d'abortar.
    # Per a runs d'inspecció pre-notarització (encara sense ticket), opt-out
    # explícit: NEXE_STRICT_SIGNING=0. (El "release-pipeline driver" que un
    # comentari antic citava aquí no ha existit mai.)
    if [[ "${NEXE_STRICT_SIGNING:-1}" == "1" ]]; then
        spctl --assess --type execute --verbose=4 "$APP" || {
            echo "❌  spctl assess ha fallat (NEXE_STRICT_SIGNING=1) — abortant build"
            exit 1
        }
    else
        spctl --assess --type execute --verbose=4 "$APP" || {
            echo "⚠️  spctl assess ha fallat — comprovar notarització"
        }
    fi
else
    # B139: fail-closed. If `cargo tauri build` exits 0 but produces no .app
    # (e.g. bundle path/name drift), the whole verification block above would be
    # skipped silently and the script would report success. Abort loudly instead.
    echo "❌  $APP no existeix — cargo tauri build no ha generat el bundle. Abortant." >&2
    exit 1
fi

echo ""
echo "=== Verificacio notarització DMG ==="
# Parameterized — we scan generated DMGs instead of assuming a name.
# Supports any version + arch (x86_64/aarch64/universal) without hardcoding.
DMG_DIR="target/release/bundle/dmg"
DMG_FAIL=0
if [[ -d "$DMG_DIR" ]]; then
    for DMG in "$DMG_DIR"/*.dmg; do
        [[ -f "$DMG" ]] || continue
        echo "Verificant $(basename "$DMG"):"
        # B138 + WSF-001 (strict per defecte): a badly notarized DMG must
        # stop the build, not just warn — a silent release with an un-notarized
        # DMG would be rejected by Gatekeeper on the user's machine.
        # Opt-out explícit per a inspecció pre-notarització: NEXE_STRICT_SIGNING=0.
        if [[ "${NEXE_STRICT_SIGNING:-1}" == "1" ]]; then
            spctl -a -t open --context context:primary-signature -v "$DMG" || {
                echo "❌  $(basename "$DMG") no notaritzat correctament (NEXE_STRICT_SIGNING=1)"
                DMG_FAIL=1
            }
        else
            spctl -a -t open --context context:primary-signature -v "$DMG" || {
                echo "⚠️  $(basename "$DMG") no notaritzat correctament"
            }
        fi
    done
else
    echo "⚠️  $DMG_DIR no existeix — cap DMG generat?"
fi

if [[ "$DMG_FAIL" -eq 1 ]]; then
    echo "❌  Notarització DMG fallida en mode estricte — abortant build"
    exit 1
fi

echo ""
echo "✅ Signing + notarization complet."
echo "Artifact: $APP"
echo "DMGs: $DMG_DIR/"

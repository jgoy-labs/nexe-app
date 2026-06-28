#!/usr/bin/env bash
# test-sign-macos-strict.sh — Regression harness for B138.
#
# B138: in strict mode (NEXE_STRICT_SIGNING=1) a badly notarized DMG
# did `spctl ... || echo "⚠️ warning"` and the build CONTINUED until
# printing "✅ Signing + notarization complet." with exit 0 → a release with
# an un-notarized DMG passed silently.
#
# This harness runs the REAL sign-macos.sh (not a sed fragment) inside a
# tmpdir with cargo/codesign/spctl stubs, where spctl FAILS for the DMG and PASSES
# for the .app, simulating exactly a badly notarized DMG.
#
# Asserts (must hold AFTER the fix; before the fix they fail → red):
#   1. exit code != 0           (the build stops)
#   2. "❌" appears in the output (visible error, not a warning)
#   3. "complet" does NOT appear (the final "✅ ... complet" is never reached)
#
# Usage: ./scripts/test-sign-macos-strict.sh   (exit 0 = test green)

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
SIGN_SCRIPT="$SCRIPT_DIR/sign-macos.sh"

if [[ ! -f "$SIGN_SCRIPT" ]]; then
    echo "FAIL: no trobo $SIGN_SCRIPT"
    exit 1
fi

TMPDIR_TEST="$(mktemp -d)"
trap 'rm -rf "$TMPDIR_TEST"' EXIT

# ── Structure that sign-macos.sh expects (relative to tmpdir/, it cds into src-tauri) ──
mkdir -p "$TMPDIR_TEST/scripts"
cp "$SIGN_SCRIPT" "$TMPDIR_TEST/scripts/sign-macos.sh"
mkdir -p "$TMPDIR_TEST/src-tauri/target/release/bundle/macos/nexe-app.app"
mkdir -p "$TMPDIR_TEST/src-tauri/target/release/bundle/dmg"
touch "$TMPDIR_TEST/src-tauri/target/release/bundle/dmg/fake.dmg"

# ── Stubs on PATH (in front) ──
STUBBIN="$TMPDIR_TEST/stubbin"
mkdir -p "$STUBBIN"

# cargo: no-op (cargo tauri build) → the artifacts are already pre-created.
cat > "$STUBBIN/cargo" <<'EOF'
#!/usr/bin/env bash
exit 0
EOF

# codesign: emits a line with "Identifier" so the script's `| grep -E`
# succeeds (with pipefail an empty grep would kill the script).
cat > "$STUBBIN/codesign" <<'EOF'
#!/usr/bin/env bash
echo "Identifier=com.test.nexe-app"
exit 0
EOF

# spctl configurable per scenario via SPCTL_DMG_RESULT:
#   "fail" → the DMG (-t open) fails (exit 1); the .app (--type execute) passes.
#   "ok"   → everything passes (exit 0).
# This way the same harness tests the bad path (B138) and the good one (happy path).
cat > "$STUBBIN/spctl" <<'EOF'
#!/usr/bin/env bash
if [[ "${SPCTL_DMG_RESULT:-fail}" == "fail" ]]; then
    for arg in "$@"; do
        [[ "$arg" == "open" ]] && exit 1   # un-notarized DMG
    done
fi
exit 0
EOF

chmod +x "$STUBBIN"/*

# Runs the REAL sign-macos.sh with a given spctl result; leaves $OUTPUT and $EXIT_CODE.
run_sign() {
    local dmg_result="$1"
    set +e
    OUTPUT="$(cd "$TMPDIR_TEST" && \
        PATH="$STUBBIN:$PATH" \
        NEXE_STRICT_SIGNING=1 \
        SPCTL_DMG_RESULT="$dmg_result" \
        TAURI_SIGNING_IDENTITY="Developer ID Application: Test (TEAMID1234)" \
        TAURI_APPLE_ID="test@example.com" \
        TAURI_APPLE_PASSWORD="@keychain:AC_PASSWORD" \
        TAURI_APPLE_TEAM_ID="TEAMID1234" \
        bash "$TMPDIR_TEST/scripts/sign-macos.sh" 2>&1)"
    EXIT_CODE=$?
    set -e
}

FAILED=0

# ── Scenario A (bug B138): badly notarized DMG + strict → must abort ──
run_sign "fail"
if [[ "$EXIT_CODE" -ne 0 ]] && echo "$OUTPUT" | grep -q "❌" && ! echo "$OUTPUT" | grep -q "complet"; then
    echo "ESCENARI A OK: DMG dolent → exit $EXIT_CODE, '❌' present, 'complet' absent"
else
    echo "ESCENARI A FAIL: esperava exit!=0 + ❌ + sense 'complet'; exit=$EXIT_CODE"
    echo "$OUTPUT"; FAILED=1
fi

# ── Scenario B (happy path): well-notarized DMG + strict → must reach 'complet' ──
# Without this case, a saboteur fix (unconditional exit 1) would also pass scenario A.
run_sign "ok"
if [[ "$EXIT_CODE" -eq 0 ]] && echo "$OUTPUT" | grep -q "complet" && ! echo "$OUTPUT" | grep -q "❌"; then
    echo "ESCENARI B OK: DMG bo → exit 0, 'complet' present, sense '❌'"
else
    echo "ESCENARI B FAIL: esperava exit 0 + 'complet' sense ❌; exit=$EXIT_CODE"
    echo "$OUTPUT"; FAILED=1
fi

echo "─────────────────────────────────────────"
if [[ "$FAILED" -eq 0 ]]; then
    echo "✅ test-sign-macos-strict: VERD"
    exit 0
else
    echo "🔴 test-sign-macos-strict: VERMELL"
    exit 1
fi

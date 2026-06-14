#!/usr/bin/env bash
# test-sign-macos-strict.sh — Harness de regressió per a B138.
#
# B138: en mode estricte (NEXE_STRICT_SIGNING=1) un DMG mal notaritzat
# feia `spctl ... || echo "⚠️ warning"` i el build CONTINUAVA fins a
# imprimir "✅ Signing + notarization complet." amb exit 0 → release amb
# DMG no notaritzat passava silenciosament.
#
# Aquest harness executa el sign-macos.sh REAL (no un fragment sed) dins un
# tmpdir amb stubs de cargo/codesign/spctl, on spctl FALLA per al DMG i PASSA
# per al .app, simulant exactament un DMG mal notaritzat.
#
# Asserts (s'han de complir DESPRÉS del fix; abans del fix fallen → red):
#   1. exit code != 0           (el build s'atura)
#   2. surt "❌" a la sortida    (error visible, no warning)
#   3. NO surt "complet"        (no s'arriba a l'"✅ ... complet" final)
#
# Ús: ./scripts/test-sign-macos-strict.sh   (exit 0 = test verd)

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
SIGN_SCRIPT="$SCRIPT_DIR/sign-macos.sh"

if [[ ! -f "$SIGN_SCRIPT" ]]; then
    echo "FAIL: no trobo $SIGN_SCRIPT"
    exit 1
fi

TMPDIR_TEST="$(mktemp -d)"
trap 'rm -rf "$TMPDIR_TEST"' EXIT

# ── Estructura que sign-macos.sh espera (relativa a tmpdir/, fa cd a src-tauri) ──
mkdir -p "$TMPDIR_TEST/scripts"
cp "$SIGN_SCRIPT" "$TMPDIR_TEST/scripts/sign-macos.sh"
mkdir -p "$TMPDIR_TEST/src-tauri/target/release/bundle/macos/nexe-app.app"
mkdir -p "$TMPDIR_TEST/src-tauri/target/release/bundle/dmg"
touch "$TMPDIR_TEST/src-tauri/target/release/bundle/dmg/fake.dmg"

# ── Stubs al PATH (davant) ──
STUBBIN="$TMPDIR_TEST/stubbin"
mkdir -p "$STUBBIN"

# cargo: no-op (cargo tauri build) → els artifacts ja estan precreats.
cat > "$STUBBIN/cargo" <<'EOF'
#!/usr/bin/env bash
exit 0
EOF

# codesign: emet una línia amb "Identifier" perquè el `| grep -E` del script
# tingui èxit (amb pipefail un grep buit mataria el script).
cat > "$STUBBIN/codesign" <<'EOF'
#!/usr/bin/env bash
echo "Identifier=com.test.nexe-app"
exit 0
EOF

# spctl configurable per escenari via SPCTL_DMG_RESULT:
#   "fail" → el DMG (-t open) falla (exit 1); el .app (--type execute) passa.
#   "ok"   → tot passa (exit 0).
# Així el mateix harness prova el camí dolent (B138) i el bo (happy path).
cat > "$STUBBIN/spctl" <<'EOF'
#!/usr/bin/env bash
if [[ "${SPCTL_DMG_RESULT:-fail}" == "fail" ]]; then
    for arg in "$@"; do
        [[ "$arg" == "open" ]] && exit 1   # DMG no notaritzat
    done
fi
exit 0
EOF

chmod +x "$STUBBIN"/*

# Executa el sign-macos.sh REAL amb un resultat d'spctl donat; deixa $OUTPUT i $EXIT_CODE.
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

# ── Escenari A (bug B138): DMG mal notaritzat + strict → ha d'avortar ──
run_sign "fail"
if [[ "$EXIT_CODE" -ne 0 ]] && echo "$OUTPUT" | grep -q "❌" && ! echo "$OUTPUT" | grep -q "complet"; then
    echo "ESCENARI A OK: DMG dolent → exit $EXIT_CODE, '❌' present, 'complet' absent"
else
    echo "ESCENARI A FAIL: esperava exit!=0 + ❌ + sense 'complet'; exit=$EXIT_CODE"
    echo "$OUTPUT"; FAILED=1
fi

# ── Escenari B (happy path): DMG ben notaritzat + strict → ha d'arribar a 'complet' ──
# Sense aquest cas, un fix sabotejador (exit 1 incondicional) també passaria l'escenari A.
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

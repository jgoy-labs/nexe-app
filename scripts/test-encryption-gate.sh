#!/usr/bin/env bash
# test-encryption-gate.sh — Harness de regressió per a B082 (CRY-01).
#
# Executa el verify-encryption-gate.sh REAL (no una rèplica) contra boot logs
# fabricats, i comprova que build-sidecar.sh el crida de debò. Si el gate
# desaparegués o es trenqués, aquest harness es posaria vermell.
#
# Casos:
#   1. server-nexe + log SENSE el missatge  → exit 1 (abort)        [captura el bug]
#   2. server-nexe + log AMB el missatge     → exit 0 + "verificat"  [happy path]
#   3. POC (APP_SOURCE_DIR buit)             → exit 0 (salta)        [no fals positiu]
#   4. build-sidecar.sh invoca el gate       → connexió present      [no codi mort]
#
# Ús: ./scripts/test-encryption-gate.sh   (exit 0 = test verd)

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
GATE="$SCRIPT_DIR/verify-encryption-gate.sh"
BUILD="$SCRIPT_DIR/build-sidecar.sh"

[[ -f "$GATE" ]]  || { echo "FAIL: no trobo $GATE"; exit 1; }
[[ -f "$BUILD" ]] || { echo "FAIL: no trobo $BUILD"; exit 1; }

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
FAILED=0

LOG_OK="$TMP/boot-ok.log"
LOG_BAD="$TMP/boot-bad.log"
printf 'booting...\nEncryption at rest: ENABLED (AES-256-GCM)\n6 routers ready\n' > "$LOG_OK"
printf 'booting...\n6 routers ready\n' > "$LOG_BAD"

# ── Cas 1: server-nexe + log sense missatge → ha d'avortar (exit 1) ──
set +e
OUT1="$("$GATE" "/some/app/dir" "$LOG_BAD" 2>&1)"; EC1=$?
set -e
if [[ "$EC1" -ne 0 ]] && echo "$OUT1" | grep -q "❌"; then
    echo "CAS 1 OK: server-nexe sense encriptació → abort (exit $EC1, ❌ present)"
else
    echo "CAS 1 FAIL: esperava exit!=0 + ❌; exit=$EC1 out='$OUT1'"; FAILED=1
fi

# ── Cas 2 (happy path): server-nexe + log amb missatge → exit 0 ──
set +e
OUT2="$("$GATE" "/some/app/dir" "$LOG_OK" 2>&1)"; EC2=$?
set -e
if [[ "$EC2" -eq 0 ]] && echo "$OUT2" | grep -q "verificat" && ! echo "$OUT2" | grep -q "❌"; then
    echo "CAS 2 OK: server-nexe amb encriptació → continua (exit 0, 'verificat')"
else
    echo "CAS 2 FAIL: esperava exit 0 + 'verificat' sense ❌; exit=$EC2 out='$OUT2'"; FAILED=1
fi

# ── Cas 3: POC (APP_SOURCE_DIR buit) → salta net (exit 0), fins i tot amb log dolent ──
set +e
OUT3="$("$GATE" "" "$LOG_BAD" 2>&1)"; EC3=$?
set -e
if [[ "$EC3" -eq 0 ]] && [[ -z "$OUT3" ]]; then
    echo "CAS 3 OK: POC → salta (exit 0, sense sortida)"
else
    echo "CAS 3 FAIL: esperava exit 0 silenciós; exit=$EC3 out='$OUT3'"; FAILED=1
fi

# ── Cas 4: build-sidecar.sh ha de cridar el gate (no codi mort) ──
if grep -q "verify-encryption-gate.sh" "$BUILD"; then
    echo "CAS 4 OK: build-sidecar.sh invoca verify-encryption-gate.sh"
else
    echo "CAS 4 FAIL: build-sidecar.sh NO crida el gate (gate desconnectat)"; FAILED=1
fi

echo "─────────────────────────────────────────"
if [[ "$FAILED" -eq 0 ]]; then
    echo "✅ test-encryption-gate: VERD"
    exit 0
else
    echo "🔴 test-encryption-gate: VERMELL"
    exit 1
fi

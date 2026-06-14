#!/usr/bin/env bash
# verify-encryption-gate.sh — B082 (CRY-01).
#
# Verifica que el boot log del sidecar server-nexe confirma encriptació-at-rest
# activa. Un build que empaqueti un sidecar sense encriptació és un defecte de
# seguretat silenciós, així que aquest gate atura el build.
#
# Només aplica al camí server-nexe (APP_SOURCE_DIR set); el camí POC no xifra i
# se salta sense error.
#
# Ús:   verify-encryption-gate.sh <APP_SOURCE_DIR> <boot_log_path>
# Exit: 0 = OK (encriptació confirmada o camí POC) · 1 = encriptació NO activa.
#
# Extret de build-sidecar.sh perquè la lògica del gate sigui testejable de debò
# (vegeu test-encryption-gate.sh) en lloc d'inspeccionar-ne una rèplica.

set -uo pipefail

APP_SOURCE_DIR="${1:-}"
BOOT_LOG="${2:-/tmp/nexe-sidecar-boot.log}"

# Camí POC (sense APP_SOURCE_DIR): no xifra → el gate no aplica.
if [ -z "$APP_SOURCE_DIR" ]; then
    exit 0
fi

if grep -q "Encryption at rest: ENABLED" "$BOOT_LOG" 2>/dev/null; then
    echo "    Encryption at rest: ENABLED (CRY-01 verificat)"
    exit 0
else
    echo "    ❌ CRY-01: encryption-at-rest NO activat al boot — abortant build"
    echo "    Sidecar boot log: $BOOT_LOG (últimes 20 línies):"
    tail -20 "$BOOT_LOG" 2>/dev/null
    exit 1
fi

#!/usr/bin/env bash
# verify-encryption-gate.sh — B082 (CRY-01).
#
# Verifies that the server-nexe sidecar boot log confirms encryption-at-rest
# is active. A build that packages a sidecar without encryption is a silent
# security defect, so this gate stops the build.
#
# Only applies to the server-nexe path (APP_SOURCE_DIR set); the POC path does not
# encrypt and is skipped without error.
#
# Usage: verify-encryption-gate.sh <APP_SOURCE_DIR> <boot_log_path>
# Exit:  0 = OK (encryption confirmed or POC path) · 1 = encryption NOT active.
#
# Extracted from build-sidecar.sh so the gate logic is genuinely testable
# (see test-encryption-gate.sh) instead of inspecting a replica of it.

set -uo pipefail

APP_SOURCE_DIR="${1:-}"
BOOT_LOG="${2:-/tmp/nexe-sidecar-boot.log}"

# POC path (no APP_SOURCE_DIR): no encryption → the gate does not apply.
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

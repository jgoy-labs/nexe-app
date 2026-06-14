#!/usr/bin/env bash
# test-privacy-gate.sh — harness REAL per a verify-privacy-gate.sh.
# Anti-teatre: executa el gate DE DEBÒ sobre escenaris en tmpdir.
#   - Escenari BRUT  (.test_data + worktrees + storage/ + *.db) → el gate ha de FALLAR.
#   - Escenari NET   (només codi, inclòs memory/memory/storage) → el gate ha de PASSAR.
# El happy path (escenari net) mata el "fix sabotejador": un gate que sempre
# falla passaria el bug scenario però fallaria aquí.
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
GATE="$SCRIPT_DIR/verify-privacy-gate.sh"
fails=0

# --- Escenari 1: BRUT → ha de FALLAR (exit != 0) ---
DIRTY="$(mktemp -d)"
mkdir -p "$DIRTY/app/.test_data/sessions" \
         "$DIRTY/app/worktrees/server-nexe-win/storage" \
         "$DIRTY/app/storage/vectors" \
         "$DIRTY/app/.github/workflows" \
         "$DIRTY/app/memory/memory/storage"
touch "$DIRTY/app/.test_data/sessions/x.enc" \
      "$DIRTY/app/storage/memory_v1.db" \
      "$DIRTY/app/worktrees/server-nexe-win/storage/system_core.db" \
      "$DIRTY/app/memory/memory/storage/sqlite_store.py"
if bash "$GATE" "$DIRTY/app" >/dev/null 2>&1; then
    echo "❌ FAIL: el gate ha PASSAT amb brossa (.test_data/worktrees/storage/*.db presents)"
    fails=$((fails+1))
else
    echo "✅ OK: el gate detecta i FALLA amb brossa"
fi
rm -rf "$DIRTY"

# --- Escenari 2: NET → ha de PASSAR (exit 0), sense fals positiu pel mòdul Python ---
CLEAN="$(mktemp -d)"
mkdir -p "$CLEAN/app/core" \
         "$CLEAN/app/memory/memory/storage" \
         "$CLEAN/app/knowledge/.embeddings"
touch "$CLEAN/app/core/main.py" \
      "$CLEAN/app/memory/memory/storage/sqlite_store.py" \
      "$CLEAN/app/knowledge/.embeddings/metadata-ca.jsonl"
if bash "$GATE" "$CLEAN/app" >/dev/null 2>&1; then
    echo "✅ OK: el gate PASSA amb un app/ net (no confon memory/memory/storage)"
else
    echo "❌ FAIL: el gate ha fallat amb un app/ net (fals positiu)"
    fails=$((fails+1))
fi
rm -rf "$CLEAN"

if [ "$fails" -gt 0 ]; then
    echo "RESULTAT: $fails test(s) del privacy-gate FALLEN"
    exit 1
fi
echo "RESULTAT: tots els tests del privacy-gate PASSEN"

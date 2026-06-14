#!/usr/bin/env bash
# verify-privacy-gate.sh — falla el build si el sidecar staged (app/) arrossega
# dades de DEV/test que NO han de viatjar al producte distribuït (B183 / B184).
#
# Per què: build-sidecar.sh Step 4 copia el working tree de DEV amb rsync per
# DENYLIST (enumera què excloure); tota denylist deixa forats — el 2026-06-14 una
# verificació adversarial va caçar que .test_data/ (315 sessions .enc) i el
# worktree worktrees/server-nexe-win/storage/ (system_core.db amb session_tokens)
# es colaven al sidecar-bundle.tar.gz distribuït. Aquest gate és la xarxa de
# seguretat DETERMINISTA: si la brossa torna a colar-se (perquè algú toca els
# excludes o afegeix un dir nou), el build PETA aquí en comptes de publicar-la.
# Contrast: el build_dmg.sh de server-nexe usa allowlist i per això és net.
#
# Ús: verify-privacy-gate.sh <APP_DIR>
#   <APP_DIR> = el directori staged que es ficarà al tarball (target/sidecar/app)
set -euo pipefail

APP_DIR="${1:?Ús: verify-privacy-gate.sh <APP_DIR>}"
if [ ! -d "$APP_DIR" ]; then
    echo "verify-privacy-gate: ERROR — APP_DIR no existeix: $APP_DIR" >&2
    exit 2
fi

violations=()

# 1) Directoris de DEV/test/CI que no han de viatjar (per nom, a qualsevol nivell).
while IFS= read -r hit; do
    [ -n "$hit" ] && violations+=("dir DEV/test/CI: $hit")
done < <(find "$APP_DIR" \( -name '.test_data' -o -name 'worktrees' \
        -o -name '.github' -o -name '.grimp_cache' -o -name '.ruff_cache' \) -print 2>/dev/null)

# 2) storage/ de runtime a l'arrel de app/ (dades). NO confondre amb el mòdul
#    Python memory/memory/storage/ (codi), que és legítim i ha de viatjar.
if [ -d "$APP_DIR/storage" ]; then
    violations+=("storage runtime a l'arrel: $APP_DIR/storage")
fi

# 3) Bases de dades de memòria/sessions (dades reals, mai codi).
while IFS= read -r hit; do
    [ -n "$hit" ] && violations+=("DB de dades: $hit")
done < <(find "$APP_DIR" \( -name 'memory_v1.db' -o -name 'system_core.db' \
        -o -name 'metadata_memory.db' -o -name 'storage.sqlite' \) -print 2>/dev/null)

if [ ${#violations[@]} -gt 0 ]; then
    echo "❌ verify-privacy-gate: el sidecar arrossega dades de DEV/test (B183/B184):" >&2
    for v in "${violations[@]}"; do echo "   - $v" >&2; done
    echo "   → revisa els excludes del rsync (build-sidecar.sh Step 4) o construeix des d'un clon net." >&2
    exit 1
fi

echo "✅ verify-privacy-gate: net (cap dada de DEV/test a $APP_DIR)"

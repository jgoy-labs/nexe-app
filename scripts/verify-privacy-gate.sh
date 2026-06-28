#!/usr/bin/env bash
# verify-privacy-gate.sh — fails the build if the staged sidecar (app/) drags in
# DEV/test data that must NOT travel into the distributed product (B183 / B184).
#
# Why: build-sidecar.sh Step 4 copies the DEV working tree with rsync via a
# DENYLIST (enumerates what to exclude); every denylist leaves holes — on 2026-06-14 an
# adversarial verification caught that .test_data/ (315 .enc sessions) and the
# worktrees/server-nexe-win/storage/ worktree (system_core.db with session_tokens)
# were sneaking into the distributed sidecar-bundle.tar.gz. This gate is the
# DETERMINISTIC safety net: if the junk sneaks back in (because someone touches the
# excludes or adds a new dir), the build BLOWS UP here instead of publishing it.
# Contrast: server-nexe's build_dmg.sh uses an allowlist, which is why it is clean.
#
# Usage: verify-privacy-gate.sh <APP_DIR>
#   <APP_DIR> = the staged directory that will go into the tarball (target/sidecar/app)
set -euo pipefail

APP_DIR="${1:?Ús: verify-privacy-gate.sh <APP_DIR>}"
if [ ! -d "$APP_DIR" ]; then
    echo "verify-privacy-gate: ERROR — APP_DIR no existeix: $APP_DIR" >&2
    exit 2
fi

violations=()

# 1) DEV/test/CI directories that must not travel (by name, at any level).
while IFS= read -r hit; do
    [ -n "$hit" ] && violations+=("dir DEV/test/CI: $hit")
done < <(find "$APP_DIR" \( -name '.test_data' -o -name 'worktrees' \
        -o -name '.github' -o -name '.grimp_cache' -o -name '.ruff_cache' \) -print 2>/dev/null)

# 2) runtime storage/ at the root of app/ (data). Do NOT confuse with the
#    Python module memory/memory/storage/ (code), which is legitimate and must travel.
if [ -d "$APP_DIR/storage" ]; then
    violations+=("storage runtime a l'arrel: $APP_DIR/storage")
fi

# 3) Memory/session databases (real data, never code).
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

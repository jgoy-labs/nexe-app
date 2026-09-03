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
# 2026-08-24 (#930): the denylist below said "clean" while the staged app/ carried
# _tmp/ (26 KB of internal notes), findings.db and four test uploads — none of them
# enumerated. Enumerating prohibited things cannot close this: the next internal file
# somebody creates is not on the list either. So when the SOURCE REPO is given, the
# gate stops asking "is this one of the known bad things?" and asks the only question
# that scales: IS THIS FILE PART OF THE PRODUCT? Anything the repository does not
# track, and that is not a declared build artefact, does not travel.
#
# Usage: verify-privacy-gate.sh <APP_DIR> [SOURCE_REPO]
#   <APP_DIR>     = the staged directory that will go into the tarball (target/sidecar/app)
#   <SOURCE_REPO> = the server-nexe checkout app/ was copied from. Given it, the gate
#                   also checks by NATURE. The build passes it right after the rsync,
#                   which is the one moment the answer is exact: nothing has been
#                   generated inside app/ yet, so every file there must be versioned.
set -euo pipefail

APP_DIR="${1:?Ús: verify-privacy-gate.sh <APP_DIR> [SOURCE_REPO]}"
SOURCE_REPO="${2:-}"
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

# 4) By NATURE (only with SOURCE_REPO): everything that travels is either tracked by
#    git or a declared build artefact. This is what catches the file nobody thought of.
if [ -n "$SOURCE_REPO" ]; then
    if ! git -C "$SOURCE_REPO" rev-parse --git-dir >/dev/null 2>&1; then
        echo "verify-privacy-gate: ERROR — SOURCE_REPO no és un repositori git: $SOURCE_REPO" >&2
        exit 2
    fi
    _tracked="$(mktemp)"; _staged="$(mktemp)"
    trap 'rm -f "$_tracked" "$_staged"' EXIT
    git -C "$SOURCE_REPO" ls-files | sort > "$_tracked"
    ( cd "$APP_DIR" && find . -type f | sed 's|^\./||' | sort ) > "$_staged"

    while IFS= read -r rel; do
        [ -n "$rel" ] || continue
        case "$rel" in
            # Declared build artefacts, each one placed by build-sidecar.sh itself.
            # A new one belongs HERE, named, and not in a wildcard that swallows the
            # next surprise with it.
            app.py) continue ;;                       # the sidecar entry point (Step: cp $APP_MODULE)
            .fastembed_cache/*) continue ;;           # pre-seeded embedding cache
            knowledge/.embeddings/*) continue ;;      # precomputed RAG vectors
            *__pycache__/*) continue ;;               # bytecode, if any survives the rsync
        esac
        violations+=("no és al repositori (ni artefacte declarat): $rel")
    done < <(comm -23 "$_staged" "$_tracked")
fi

if [ ${#violations[@]} -gt 0 ]; then
    echo "❌ verify-privacy-gate: el sidecar arrossega el que no és producte (B183/B184/#930):" >&2
    for v in "${violations[@]}"; do echo "   - $v" >&2; done
    echo "   → revisa els excludes del rsync (build-sidecar.sh Step 4) o construeix des d'un clon net." >&2
    exit 1
fi

if [ -n "$SOURCE_REPO" ]; then
    echo "✅ verify-privacy-gate: net — tot el que viatja és del repositori o artefacte declarat ($APP_DIR)"
else
    echo "✅ verify-privacy-gate: net (cap dada de DEV/test a $APP_DIR)"
fi

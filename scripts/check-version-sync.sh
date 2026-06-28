#!/usr/bin/env bash
# check-version-sync.sh — CI-H02
# Verifies that the project's version sources declare the same value.
#
# Sources checked:
#   1. nexe-app/package.json              ("version")
#   2. nexe-app/src-tauri/Cargo.toml      ([package] version)
#   3. nexe-app/src-tauri/tauri.conf.json ("version")
#   4. server-nexe/pyproject.toml         (version = "…")  — if accessible
#
# Usage:
#   bash scripts/check-version-sync.sh                          # tries to locate server-nexe automatically
#   bash scripts/check-version-sync.sh /path/to/server-nexe    # explicit path to the sibling repo
#   bash scripts/check-version-sync.sh --skip-server-nexe      # skips source 4 (nexe-app CI without a server-nexe checkout)
#
# Returns exit 0 if all checked sources match,
#          exit 1 with detailed diagnostics if they diverge.
#
# Compatible with bash 3.x (macOS) and bash 4+/5 (Linux/CI).

set -euo pipefail

# ---- Flags ----

SKIP_SERVER_NEXE=false
SERVER_NEXE_ARG=""

for arg in "$@"; do
    case "$arg" in
        --skip-server-nexe) SKIP_SERVER_NEXE=true ;;
        -*)
            echo "ERROR: argument desconegut: $arg"
            exit 1
            ;;
        *) SERVER_NEXE_ARG="$arg" ;;
    esac
done

# ---- Path resolution ----
# The script can run from the nexe-app root or from scripts/

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

PACKAGE_JSON="$REPO_ROOT/package.json"
CARGO_TOML="$REPO_ROOT/src-tauri/Cargo.toml"
TAURI_CONF="$REPO_ROOT/src-tauri/tauri.conf.json"

if [[ "$SKIP_SERVER_NEXE" == false ]]; then
    if [[ -n "$SERVER_NEXE_ARG" ]]; then
        # Explicit path (absolute or relative to the runner's cwd, not REPO_ROOT)
        SERVER_NEXE_ROOT="$(cd "$SERVER_NEXE_ARG" 2>/dev/null && pwd)" || {
            echo "ERROR: no s'ha pogut accedir a server-nexe a '$SERVER_NEXE_ARG'"
            exit 1
        }
    else
        # Local monorepo convention: server-nexe next to nexe-app
        SERVER_NEXE_ROOT="$(cd "$REPO_ROOT/.." && pwd)/server-nexe"
    fi
    PYPROJECT="$SERVER_NEXE_ROOT/pyproject.toml"
fi

# ---- Extraction functions ----

extract_package_json() {
    # We read from stdin (not from a path): on Windows MSYS bash opens the /d/a/... path
    # but native python3 does not understand MSYS paths. stdin avoids passing it any path.
    python3 -c "import json,sys; print(json.load(sys.stdin)['version'])" < "$PACKAGE_JSON"
}

extract_cargo_toml() {
    # Reads the first 'version = "…"' line of the [package] section.
    # Note: /^\[package\]/,/^\[/ does not work in BSD/macOS awk because the end
    # pattern /^\[/ matches the same start line, collapsing the range.
    # We use a manual accumulator to avoid that.
    awk '/^\[package\]/{found=1} found && /^\[/ && !/^\[package\]/{exit} found{print}' "$CARGO_TOML" \
        | grep -m1 '^version' \
        | sed 's/version = "\(.*\)"/\1/'
}

extract_tauri_conf() {
    # See extract_package_json: stdin for Windows compatibility (MSYS bash + native python).
    python3 -c "import json,sys; print(json.load(sys.stdin)['version'])" < "$TAURI_CONF"
}

extract_pyproject() {
    # Format PEP 621 / uv: version = "1.2.3"
    grep -m1 '^version' "$PYPROJECT" | sed 's/version = "\(.*\)"/\1/'
}

# ---- Version collection (simple variables for bash 3 compatibility) ----

V_PACKAGE_JSON="$(extract_package_json)"
V_CARGO_TOML="$(extract_cargo_toml)"
V_TAURI_CONF="$(extract_tauri_conf)"
V_PYPROJECT=""

CHECK_PYPROJECT=false
if [[ "$SKIP_SERVER_NEXE" == true ]]; then
    echo "INFO: comprovacio de server-nexe/pyproject.toml omesa (--skip-server-nexe)"
elif [[ -f "$PYPROJECT" ]]; then
    V_PYPROJECT="$(extract_pyproject)"
    CHECK_PYPROJECT=true
else
    echo "AVIS: no s'ha trobat server-nexe/pyproject.toml a '${PYPROJECT:-<no definit>}'"
    echo "      Passa el path com a argument o usa --skip-server-nexe per ometre'l."
fi

# ---- Comparison ----

echo ""
echo "=== Versions detectades ==="
printf "  %-45s %s\n" "nexe-app/package.json"              "$V_PACKAGE_JSON"
printf "  %-45s %s\n" "nexe-app/src-tauri/Cargo.toml"      "$V_CARGO_TOML"
printf "  %-45s %s\n" "nexe-app/src-tauri/tauri.conf.json" "$V_TAURI_CONF"
if [[ "$CHECK_PYPROJECT" == true ]]; then
    printf "  %-45s %s\n" "server-nexe/pyproject.toml"     "$V_PYPROJECT"
fi
echo ""

# Builds a list of values to compare
ALL_VERSIONS="$V_PACKAGE_JSON
$V_CARGO_TOML
$V_TAURI_CONF"

if [[ "$CHECK_PYPROJECT" == true ]]; then
    ALL_VERSIONS="$ALL_VERSIONS
$V_PYPROJECT"
fi

UNIQUE_VERSIONS=$(printf '%s\n' "$ALL_VERSIONS" | sort -u)
COUNT=$(printf '%s\n' "$ALL_VERSIONS" | sort -u | wc -l | tr -d ' ')

if [[ "$COUNT" -eq 1 ]]; then
    echo "OK: totes les fonts comprovades declaren la versio $(printf '%s' "$UNIQUE_VERSIONS")"
    exit 0
else
    echo "ERROR: les fonts de versio no coincideixen!"
    echo ""
    printf "  %-45s -> %s\n" "nexe-app/package.json"              "$V_PACKAGE_JSON"
    printf "  %-45s -> %s\n" "nexe-app/src-tauri/Cargo.toml"      "$V_CARGO_TOML"
    printf "  %-45s -> %s\n" "nexe-app/src-tauri/tauri.conf.json" "$V_TAURI_CONF"
    if [[ "$CHECK_PYPROJECT" == true ]]; then
        printf "  %-45s -> %s\n" "server-nexe/pyproject.toml"     "$V_PYPROJECT"
    fi
    echo ""
    echo "  Sincronitza totes les fonts abans de fer merge."
    exit 1
fi

#!/usr/bin/env bash
# check-version-sync.sh — CI-H02
# Verifica que les fonts de versió del projecte declaren el mateix valor.
#
# Fonts comprovades:
#   1. nexe-app/package.json              ("version")
#   2. nexe-app/src-tauri/Cargo.toml      ([package] version)
#   3. nexe-app/src-tauri/tauri.conf.json ("version")
#   4. server-nexe/pyproject.toml         (version = "…")  — si accessible
#
# Ús:
#   bash scripts/check-version-sync.sh                          # intenta localitzar server-nexe automàticament
#   bash scripts/check-version-sync.sh /ruta/al/server-nexe    # path explícit al repo germà
#   bash scripts/check-version-sync.sh --skip-server-nexe      # omet la font 4 (CI de nexe-app sense checkout de server-nexe)
#
# Retorna exit 0 si totes les fonts comprovades coincideixen,
#          exit 1 amb diagnòstic detallat si divergeixen.
#
# Compatible amb bash 3.x (macOS) i bash 4+/5 (Linux/CI).

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

# ---- Resolució de paths ----
# L'script pot executar-se des de l'arrel de nexe-app o des de scripts/

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

PACKAGE_JSON="$REPO_ROOT/package.json"
CARGO_TOML="$REPO_ROOT/src-tauri/Cargo.toml"
TAURI_CONF="$REPO_ROOT/src-tauri/tauri.conf.json"

if [[ "$SKIP_SERVER_NEXE" == false ]]; then
    if [[ -n "$SERVER_NEXE_ARG" ]]; then
        # Path explícit (absolut o relatiu al cwd del runner, no al REPO_ROOT)
        SERVER_NEXE_ROOT="$(cd "$SERVER_NEXE_ARG" 2>/dev/null && pwd)" || {
            echo "ERROR: no s'ha pogut accedir a server-nexe a '$SERVER_NEXE_ARG'"
            exit 1
        }
    else
        # Convenció de monorepo local: server-nexe al costat de nexe-app
        SERVER_NEXE_ROOT="$(cd "$REPO_ROOT/.." && pwd)/server-nexe"
    fi
    PYPROJECT="$SERVER_NEXE_ROOT/pyproject.toml"
fi

# ---- Funcions d'extracció ----

extract_package_json() {
    python3 -c "import json; print(json.load(open('$PACKAGE_JSON'))['version'])"
}

extract_cargo_toml() {
    # Llegeix la primera línia 'version = "…"' de la secció [package].
    # Nota: /^\[package\]/,/^\[/ no funciona en awk BSD/macOS perquè el pattern
    # de fi /^\[/ coincideix amb la mateixa línia d'inici, col·lapsant el rang.
    # Usem un acumulador manual per evitar-ho.
    awk '/^\[package\]/{found=1} found && /^\[/ && !/^\[package\]/{exit} found{print}' "$CARGO_TOML" \
        | grep -m1 '^version' \
        | sed 's/version = "\(.*\)"/\1/'
}

extract_tauri_conf() {
    python3 -c "import json; print(json.load(open('$TAURI_CONF'))['version'])"
}

extract_pyproject() {
    # Format PEP 621 / uv: version = "1.2.3"
    grep -m1 '^version' "$PYPROJECT" | sed 's/version = "\(.*\)"/\1/'
}

# ---- Recull de versions (variables simples per compatibilitat bash 3) ----

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

# ---- Comparació ----

echo ""
echo "=== Versions detectades ==="
printf "  %-45s %s\n" "nexe-app/package.json"              "$V_PACKAGE_JSON"
printf "  %-45s %s\n" "nexe-app/src-tauri/Cargo.toml"      "$V_CARGO_TOML"
printf "  %-45s %s\n" "nexe-app/src-tauri/tauri.conf.json" "$V_TAURI_CONF"
if [[ "$CHECK_PYPROJECT" == true ]]; then
    printf "  %-45s %s\n" "server-nexe/pyproject.toml"     "$V_PYPROJECT"
fi
echo ""

# Construeix llista de valors a comparar
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

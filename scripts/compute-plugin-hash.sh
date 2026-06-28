#!/usr/bin/env bash
# compute-plugin-hash.sh — Computes the SHA-256 of a plugin per ADR-0014.
# Usage: ./scripts/compute-plugin-hash.sh <plugin_dir>
# Example: ./scripts/compute-plugin-hash.sh plugins-dev/rag
#
# Copy the output into the `[integrity].sha256` field of the plugin's manifest.toml.

set -euo pipefail

if [[ $# -ne 1 ]]; then
    echo "Usage: $0 <plugin_dir>" >&2
    exit 2
fi

PLUGIN_DIR="$1"
if [[ ! -d "$PLUGIN_DIR" ]]; then
    echo "error: not a directory: $PLUGIN_DIR" >&2
    exit 2
fi

cd "$(dirname "$0")/.."
ABS_PLUGIN="$(cd "$PLUGIN_DIR" && pwd)"

cd src-tauri
cargo run --quiet --bin plugin-hash -- "$ABS_PLUGIN"

#!/usr/bin/env bash
# win-remote-build.sh — runs ON the Windows ARM64 target (git-bash), shipped FRESH from
# the Mac on every build by build-windows-installer.sh.
#
# Design contract (why this lives on the Mac, not the target):
#   The TOOL and the SOURCE OF TRUTH live on the Mac (versioned in this repo). The Windows
#   box is a DISPOSABLE test target: wipe it and re-run — nothing is lost. This script
#   therefore takes the two source bundles the orchestrator just shipped, refreshes the
#   working copies to the EXACT commits, and builds. It keeps no state the Mac can't rebuild.
#
# Args:  $1 = server-nexe commit   $2 = nexe-app commit
#
# Target layout (one-time prereqs — see docs/build-windows-arm64.md):
#   /c/nexe/python          python-build-standalone (aarch64) — NEXE_WIN_PBS
#   /c/nexe/_incoming       where the orchestrator scp's the bundles + this script
#   $HOME/.local/bin (uv)   $HOME/.cargo/bin (rust)
#   MSVC arm64 (vcvarsall link.exe) + LLVM on PATH  ← required for the `tauri build` step
set -euo pipefail

SRV_COMMIT="${1:?usage: win-remote-build.sh <server-nexe-commit> <nexe-app-commit> [skip-sidecar]}"
APP_COMMIT="${2:?usage: win-remote-build.sh <server-nexe-commit> <nexe-app-commit> [skip-sidecar]}"
SKIP_SIDECAR="${3:-}"   # non-empty → reuse an existing sidecar tarball (fast tauri iteration)

INCOMING=/c/nexe/_incoming
SRV_SRC=/c/nexe/srv-build              # server-nexe source (APP_SOURCE_DIR for the sidecar)
APP_SRC=$HOME/nexe-app-win     # nexe-app repo (we build from here)
PBS=/c/nexe/python

log() { printf '\n== %s ==\n' "$*"; }

# --- 1. Refresh both working copies from the shipped bundles → exact commit, verifiable.
#     A bundle carries its commits, so `git fetch <bundle>` + `checkout <commit>` pins the
#     tree deterministically. If the dir is a plain git-archive (no .git, the old stale
#     srv-build), we re-clone from the bundle so it becomes a real, refreshable checkout.
refresh() {
  local bundle="$1" dir="$2" commit="$3"
  [[ -f "$bundle" ]] || { echo "FATAL: bundle missing: $bundle"; exit 2; }
  if [[ ! -d "$dir/.git" ]]; then
    git clone --no-checkout "$bundle" "$dir"
  else
    # Fetch into remote-tracking refs (NEVER refs/heads/* — git refuses to fetch into the
    # branch checked out in the nexe-app clone). Then hard-detach onto the exact commit.
    git -C "$dir" fetch --force "$bundle" 'refs/heads/*:refs/remotes/bundle/*'
  fi
  git -C "$dir" checkout --force --detach "$commit"
  git -C "$dir" --no-pager log -1 --oneline
}
log "refresh server-nexe → $SRV_COMMIT"; refresh "$INCOMING/server-nexe-win.bundle" "$SRV_SRC" "$SRV_COMMIT"
log "refresh nexe-app   → $APP_COMMIT"; refresh "$INCOMING/nexe-app-win.bundle"   "$APP_SRC" "$APP_COMMIT"

# --- 2. GATE (anti-stale, source): the Ollama Windows fix MUST be in the refreshed source.
#     This is the guard that turns the old silent-stale failure into a loud one.
log "gate: Ollama fix present in source"
grep -q "_install_ollama_windows" "$SRV_SRC/installer/installer_ollama_install.py" \
  || { echo "FATAL: _install_ollama_windows missing from source $SRV_COMMIT — stale build refused"; exit 3; }

# --- 3+4. Build the Python sidecar (venv + PBS + app + fastembed) and package it into
#     src-tauri/sidecar-bundle.tar.gz. Skippable (reuse existing tarball) for fast tauri iteration.
cd "$APP_SRC"
export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$PATH"

# git-bash -lc puts /usr/bin FIRST, whose link.exe is the coreutils `link` (a
# hardlink tool), NOT the MSVC linker — cargo's link step then fails on every
# build script. Put the MSVC arm64 bin dir in front for the WHOLE build. This
# also bites the SIDECAR build: fastembed's py-rust-stemmers is a native wheel
# that compiles via cargo, so a cold uv cache (e.g. after a disk cleanup) forces
# a from-source build here — previously only the tauri step (below) was guarded.
MSVC_LINK=$(find "/c/Program Files (x86)/Microsoft Visual Studio/2022/BuildTools/VC/Tools/MSVC" \
  -ipath "*/bin/Hostarm64/arm64/link.exe" 2>/dev/null | sort -V | tail -1)
[[ -n "$MSVC_LINK" ]] || { echo "FATAL: MSVC arm64 link.exe not found (VS BuildTools + arm64 component?)"; exit 7; }
export PATH="$(dirname "$MSVC_LINK"):$PATH"
echo "link.exe -> $(command -v link.exe)"
case "$(command -v link.exe)" in /usr/bin/*|/bin/*) echo "FATAL: link.exe still coreutils, not MSVC"; exit 7 ;; esac

if [[ -n "$SKIP_SIDECAR" && -f src-tauri/sidecar-bundle.tar.gz ]]; then
  log "SKIP_SIDECAR: reusing existing src-tauri/sidecar-bundle.tar.gz"
else
  log "build-sidecar"
  APP_SOURCE_DIR="$SRV_SRC" REQUIREMENTS="$SRV_SRC/requirements.txt" NEXE_WIN_PBS="$PBS" \
    bash scripts/build-sidecar.sh
  log "pre-bundle-sidecar"
  bash scripts/pre-bundle-sidecar.sh
fi

# --- 5. GATE (anti-stale, tarball): the fix is baked into the sealed bundle, no repack.
log "gate: Ollama fix baked in tarball"
TB=src-tauri/sidecar-bundle.tar.gz
tar -xzOf "$TB" app/installer/installer_ollama_install.py | grep -q "_install_ollama_windows" \
  || { echo "FATAL: fix missing from $TB — the build did not bake the source (repack drift?)"; exit 4; }
# Belt-and-braces cp1252: the Windows install fn must stay emoji-free (its stdout is not UTF-8
# until PYTHONUTF8 lands at spawn; a raw emoji print would still crash the installer step).
if tar -xzOf "$TB" app/installer/installer_ollama_install.py \
     | awk '/def _install_ollama_windows/,/^def [a-z]/' \
     | grep -qP '[\x{1F000}-\x{1FAFF}\x{2600}-\x{27BF}]'; then
  echo "FATAL: emoji found inside _install_ollama_windows — cp1252 crash risk"; exit 5
fi

# --- 6. JS deps: the tauri CLI + the rolldown arm64 binding (REPRO-1) live in node_modules;
#     a fresh checkout has none, so `pnpm tauri` would fail with "tauri not recognized".
log "pnpm install (tauri CLI + rolldown arm64 binding)"
command -v pnpm >/dev/null 2>&1 || { echo "FATAL: pnpm not on PATH"; exit 6; }
pnpm install --frozen-lockfile

# --- 7. Build the NSIS installer. The MSVC arm64 linker was already put in front
#     of PATH above (before the sidecar build), so cargo's link step here uses it
#     instead of git-bash's coreutils /usr/bin/link.exe.
log "tauri build --bundles nsis"
pnpm tauri build --bundles nsis

# --- 7. Report the installer artifact.
log "installer"
INSTALLER=$(ls -t src-tauri/target/*/release/bundle/nsis/*-setup.exe 2>/dev/null | head -1 || true)
[[ -n "$INSTALLER" ]] || INSTALLER=$(ls -t src-tauri/target/release/bundle/nsis/*-setup.exe 2>/dev/null | head -1 || true)
[[ -n "$INSTALLER" ]] || { echo "FATAL: no NSIS installer produced"; exit 6; }
echo "INSTALLER=$INSTALLER"
( sha256sum "$INSTALLER" 2>/dev/null || shasum -a 256 "$INSTALLER" ) | awk '{print "SHA256="$1}'
du -h "$INSTALLER" | awk '{print "SIZE="$1}'
echo "OK: Windows ARM64 installer built from source (no stale, no repack)."

# --- 8. Stage the artifact for retrieval (the Mac pulls it back — the VM owns NOTHING).
#     Fixed staging dir + manifest so the orchestrator never has to guess paths, and a
#     Desktop copy so the smoke-tester can double-click without hunting through target/.
log "stage artifact for Mac retrieval"
OUTGOING=/c/nexe/_outgoing
mkdir -p "$OUTGOING"
rm -f "$OUTGOING"/*-setup.exe "$OUTGOING"/INSTALLER.txt
cp "$INSTALLER" "$OUTGOING/"
BASE=$(basename "$INSTALLER")
SHA=$( (sha256sum "$OUTGOING/$BASE" 2>/dev/null || shasum -a 256 "$OUTGOING/$BASE") | awk '{print $1}')
printf 'NAME=%s\nSHA256=%s\n' "$BASE" "$SHA" > "$OUTGOING/INSTALLER.txt"
rm -f $HOME/Desktop/*-setup.exe || true   # kill stale installers — the Desktop only ever holds THIS build
cp "$INSTALLER" $HOME/Desktop/ || echo "WARN: Desktop copy failed (non-fatal)"
echo "staged: $OUTGOING/$BASE (+ Desktop copy for smoke-test)"

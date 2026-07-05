#!/usr/bin/env bash
# build-windows-installer.sh — Mac-side orchestrator for the Windows ARM64 NSIS installer.
#
# THE TOOL LIVES ON THE MAC. The Windows box is a disposable test target: this script ships
# FRESH source (git bundles of the two win branches) + the remote build script to the target,
# builds there, and leaves the installer on the target for smoke-testing. Wipe the target and
# re-run — nothing is lost, because the source of truth is here.
#
# This mirrors the macOS DMG build: same product (sidecar + Tauri), only the
# compilation target differs. The one Windows-specific twist is that the build runs on a
# remote ARM64 box (there is no cross-compiled functional sidecar — see release.yml B053).
#
# Usage:
#   scripts/build-windows-installer.sh [--target user@host] [--srv-dir DIR] [--dry-run]
#
# Env overrides: TARGET (or NEXE_WIN_BUILD_TARGET), SRV_WT (or NEXE_WIN_SRV_WT), STAGE (Mac staging dir).
set -euo pipefail

# Optional per-machine config (gitignored, never published): sets TARGET / SRV_WT
# for this developer's box so the defaults below stay generic and public-safe.
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
[ -f "$SCRIPT_DIR/build-windows.local" ] && . "$SCRIPT_DIR/build-windows.local"

TARGET="${TARGET:-${NEXE_WIN_BUILD_TARGET:-user@windows-arm64-host}}"
REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"                       # nexe-app-win worktree (Mac)
SRV_WT="${SRV_WT:-${NEXE_WIN_SRV_WT:-$HOME/nexe-src/server-nexe}}"
STAGE="${STAGE:-/tmp/nexe-win-build}"
SRV_BRANCH=server-nexe-win
APP_BRANCH=nexe-app-win
REMOTE_INCOMING_POSIX=/c/nexe/_incoming   # git-bash sees this (POSIX)
REMOTE_INCOMING_WIN='C:/nexe/_incoming'   # the Windows sshd SFTP wants this (rejects /c/…)
DRY_RUN=0
SKIP_SIDECAR=          # --skip-sidecar → reuse the target's existing sidecar tarball

while [[ $# -gt 0 ]]; do
  case "$1" in
    --target)  TARGET="$2"; shift 2 ;;
    --srv-dir) SRV_WT="$2"; shift 2 ;;
    --dry-run) DRY_RUN=1; shift ;;
    --skip-sidecar) SKIP_SIDECAR=skip; shift ;;
    *) echo "unknown arg: $1"; exit 1 ;;
  esac
done

# --- 1. Resolve the exact commits we are about to ship.
SRV_COMMIT=$(git -C "$SRV_WT" rev-parse "$SRV_BRANCH")
APP_COMMIT=$(git -C "$REPO_ROOT" rev-parse "$APP_BRANCH")
echo "server-nexe $SRV_BRANCH → $SRV_COMMIT"
echo "nexe-app    $APP_BRANCH → $APP_COMMIT"

# --- 2. Mac-side gate: the Ollama fix must be in the source we are about to bundle.
grep -q "_install_ollama_windows" "$SRV_WT/installer/installer_ollama_install.py" \
  || { echo "FATAL: _install_ollama_windows not in $SRV_WT — nothing worth shipping"; exit 2; }

# --- 3. Build the bundles (carry the commits) + collect the remote script.
mkdir -p "$STAGE"
git -C "$SRV_WT"    bundle create "$STAGE/server-nexe-win.bundle" "$SRV_BRANCH"
git -C "$REPO_ROOT" bundle create "$STAGE/nexe-app-win.bundle"    "$APP_BRANCH"
cp "$REPO_ROOT/scripts/win-remote-build.sh" "$REPO_ROOT/scripts/win-build-env.bat" "$STAGE/"
echo "staged bundles + remote script + env wrapper in $STAGE"

if [[ $DRY_RUN -eq 1 ]]; then
  echo "--dry-run: stopping before touching $TARGET"; exit 0
fi

# --- 4. Ship to the target (stateless: the target only ever receives, never owns).
ssh "$TARGET" "\"C:\\Program Files\\Git\\bin\\bash.exe\" -lc \"mkdir -p $REMOTE_INCOMING_POSIX\""
scp "$STAGE/server-nexe-win.bundle" "$STAGE/nexe-app-win.bundle" \
    "$STAGE/win-remote-build.sh" "$STAGE/win-build-env.bat" \
    "$TARGET:$REMOTE_INCOMING_WIN/"

# --- 5. Build on the target via the env wrapper (loads MSVC arm64 + LLVM, then git-bash).
#     The .bat encapsulates the space-heavy Windows paths so we avoid quoting hell over ssh.
ssh "$TARGET" "C:\\nexe\\_incoming\\win-build-env.bat $SRV_COMMIT $APP_COMMIT $SKIP_SIDECAR"

# --- 6. Retrieve the artifact (the Mac owns the artifact too — the VM stays disposable).
#     The remote build staged installer + manifest in C:/nexe/_outgoing; pull and verify.
ARTIFACTS="$REPO_ROOT/win-artifacts"   # NOT under target/ — a cargo clean must never eat a verified artifact
mkdir -p "$ARTIFACTS"
MANIFEST=$(ssh "$TARGET" "type C:\\nexe\\_outgoing\\INSTALLER.txt") \
  || { echo "FATAL: no INSTALLER.txt manifest on $TARGET (build did not stage the artifact?)"; exit 8; }
NAME=$(printf '%s\n' "$MANIFEST" | sed -n 's/^NAME=//p' | tr -d '\r')
SHA_REMOTE=$(printf '%s\n' "$MANIFEST" | sed -n 's/^SHA256=//p' | tr -d '\r')
[[ -n "$NAME" && -n "$SHA_REMOTE" ]] || { echo "FATAL: unreadable INSTALLER.txt manifest on $TARGET"; exit 8; }
[[ "$NAME" == *-setup.exe && "$NAME" != */* ]] || { echo "FATAL: suspicious NAME from manifest: $NAME"; exit 8; }
scp "$TARGET:C:/nexe/_outgoing/$NAME" "$ARTIFACTS/"
SHA_LOCAL=$(shasum -a 256 "$ARTIFACTS/$NAME" | awk '{print $1}')
[[ "$SHA_LOCAL" == "$SHA_REMOTE" ]] || { echo "FATAL: SHA mismatch after transfer ($SHA_LOCAL != $SHA_REMOTE)"; exit 8; }
printf '%s  %s\n' "$SHA_LOCAL" "$NAME" > "$ARTIFACTS/$NAME.sha256"

echo ""
echo "Done. Artifact retrieved and verified:"
echo "  Mac copy : $ARTIFACTS/$NAME"
echo "  SHA256   : $SHA_LOCAL"
echo "  VM copies: C:/nexe/_outgoing/$NAME + Desktop (double-click to smoke-test:"
echo "  onboarding → auto-install Ollama → pull model → chat responds)."

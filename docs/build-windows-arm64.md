# Build — Windows ARM64 (NSIS installer)

> Native build procedure for the Windows on ARM (aarch64-pc-windows-msvc) NSIS
> installer. The CI release pipeline (`.github/workflows/release.yml`) cross-compiles
> the ARM64 target from a `windows-latest` (x64) runner; this document covers a
> **native** ARM64 build on a Windows 11 ARM64 machine (e.g. the UTM VM), which needs
> a few host prerequisites the CI runner already ships.

## The tool: `scripts/build-windows-installer.sh` (Mac-driven — recommended)

One command on the **Mac** drives the whole build; the Windows box is a **disposable test
target** (the tool and the source of truth live on the Mac — wipe the target and re-run,
nothing is lost). This is the Windows sibling of the macOS DMG build: same product (sidecar +
Tauri), only the compile target differs.

```bash
# From the Mac, in the nexe-app worktree (server-nexe-win + nexe-app-win must be committed):
scripts/build-windows-installer.sh [--target user@host]   # default user@windows-arm64-host (or set NEXE_WIN_BUILD_TARGET)
scripts/build-windows-installer.sh --dry-run              # stage bundles only, don't touch the target
```

Pipeline (all driven from the Mac):
1. Resolve the current `server-nexe-win` + `nexe-app-win` commits, `git bundle` both, and
   gate (Mac-side) that the source carries the Ollama Windows fix.
2. `scp` the two bundles + `win-remote-build.sh` + `win-build-env.bat` to the target's
   `C:/nexe/_incoming`.
3. On the target, `win-build-env.bat` loads MSVC arm64 (vcvarsall) + LLVM, then git-bash runs
   `win-remote-build.sh`, which:
   - refreshes `srv-build` and `nexe-app-win` **from the bundles** to the exact commits
     (never stale — this replaces the old hand-copied `git archive`);
   - gates the Ollama fix in the source **and** in the sealed `sidecar-bundle.tar.gz`
     (no repack drift), plus a cp1252 emoji check;
   - runs `build-sidecar.sh` → `pre-bundle-sidecar.sh` → `tauri build --bundles nsis`.
4. Leaves the NSIS installer on the target; smoke-test it there (onboarding → auto-install
   Ollama → pull model → chat responds).

**Why the source can't go stale:** every run re-derives the target's source from a fresh
bundle of the committed branch, and the grep gates turn any stale/repack drift into a hard
fail instead of a silently-wrong installer (the 2026-07-01 failure mode).

Gotchas the tool already handles (kept here for when you touch it):
- **scp path format:** the Windows sshd SFTP rejects `/c/…` and wants `C:/…`; git-bash inside
  the build wants `/c/…`. The orchestrator uses each in its place.
- **refresh into a checked-out branch:** `git fetch …:refs/heads/*` is refused for the branch
  checked out in the `nexe-app-win` clone — fetch into `refs/remotes/bundle/*` + `checkout --detach`.
- **MSVC env:** `tauri build` needs `link.exe`/clang, which `build-run.bat` never loaded;
  `win-build-env.bat` calls vcvarsall arm64 + prepends LLVM before git-bash.

The manual steps below remain the low-level reference (what the tool automates).

## Host prerequisites

| Tool | Notes |
|------|-------|
| **Rust** | Toolchain pinned by `rust-toolchain.toml` (`1.94.1`). Install target: `rustup target add aarch64-pc-windows-msvc`. |
| **VS 2022 Build Tools** | Must include the **MSVC ARM64** component (`link.exe` under `VC\Tools\MSVC\<ver>\bin\HostARM64\arm64`). Not on `PATH` by default — see below. |
| **LLVM / clang** | Required to compile `ring` (crypto backend via rustls/reqwest) for `aarch64-pc-windows-msvc`; MSVC `armasm64` cannot assemble its perlasm. Install LLVM and add `C:\Program Files\LLVM\bin` to `PATH`. |
| **Node + pnpm** | `pnpm install` (the `@rolldown/binding-win32-arm64-msvc` binding is kept in `pnpm-workspace.yaml`, so a clean install fetches it — see REPRO-1 note below). |
| **uv** | `curl -LsSf https://astral.sh/uv/install.sh | sh` (git-bash). Used by `build-sidecar.sh` to create the venv. |
| **Python (PBS)** | uv has **no** windows-aarch64 python-build-standalone → download it manually. See `NEXE_WIN_PBS` below. |
| **Disk** | ~**4–5 GB** free on `C:` (installer ~1.37 GB + `sidecar-bundle.tar.gz` ~1.27 GB + build intermediates). Watch out: a full build can end with very little headroom. |

### MSVC environment

`link.exe` is not on the default `PATH`. Either:

- Run the whole build from the **"ARM64 Native Tools Command Prompt for VS 2022"** (it loads `vcvars`), **or**
- `call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvarsall.bat" arm64`
  at the start of your build shell.

### `NEXE_WIN_PBS` — the ARM64 Python runtime

`scripts/build-sidecar.sh` requires `NEXE_WIN_PBS` to point at an extracted
[python-build-standalone](https://github.com/astral-sh/python-build-standalone/releases)
build for **cpython 3.12 / aarch64-pc-windows-msvc** (`PY_VERSION=3.12` in the script).
Extract it so that `%NEXE_WIN_PBS%\python.exe` exists, e.g. `C:\nexe\python`.

## Build steps

```bash
# From a shell with the MSVC arm64 env + LLVM on PATH (see above):

# 1. Frontend + JS deps (fetches the rolldown arm64 binding)
pnpm install

# 2. Python sidecar bundle (venv + PBS + app + fastembed) → target/sidecar/
APP_SOURCE_DIR=/c/nexe/srv-build \
REQUIREMENTS=/c/nexe/srv-build/requirements.txt \
NEXE_WIN_PBS=/c/nexe/python \
  bash scripts/build-sidecar.sh

# 3. Package the sidecar into src-tauri/sidecar-bundle.tar.gz (+ .sha256)
bash scripts/pre-bundle-sidecar.sh

# 4. Build the NSIS installer (Tauri auto-merges tauri.windows.conf.json → target nsis)
pnpm tauri build --bundles nsis
```

Output: `src-tauri/target/release/bundle/nsis/nexe-app_<version>_arm64-setup.exe`.

The installer is **unsigned** by default (SmartScreen will warn). Code signing is
gated behind `WINDOWS_SIGNING_CERT` (`scripts/sign-sidecar-binaries.ps1`).

## Notes / gotchas

- **REPRO-1 (rolldown binding):** vite 8 uses rolldown, whose native binding is a
  platform-specific optional dependency. `@rolldown/binding-win32-arm64-msvc` is kept
  in `pnpm-workspace.yaml` (not in the skip list) so `pnpm install` on ARM64 fetches it;
  without it `vite build` fails with `Cannot find module '@rolldown/binding-win32-arm64-msvc'`.
- **WebView2:** `tauri.windows.conf.json` uses `downloadBootstrapper` (arch-aware).
  Win11 ARM64 ships WebView2 Evergreen preinstalled, so the bootstrapper is a no-op at
  install time. (Avoid `offlineInstaller` on ARM64 — Tauri's bundler has no arm64 offline
  URL and falls back to embedding the ~127 MB x86 runtime.)
- **SSH into cmd.exe:** avoid pipes `|`, `%errorlevel%` and `if exist` on a single line
  (they return empty); use `&` chains and redirect to a log file.
- **git-bash lies about `uname -m`** (reports x86_64 under emulation) — `build-sidecar.sh`
  forces `aarch64` via a `case` on Windows.

#!/bin/bash
# ────────────────────────────────────────────────────────────────────────
# build-sidecar.sh
# POC: Build a self-contained Python sidecar using python-build-standalone
# (via uv) without requiring system Python or a pre-existing venv.
#
# Approach: PBS + uv (evolution of server-nexe's build-python-bundle.sh)
# ADR-0016 documents the decision.
# ────────────────────────────────────────────────────────────────────────
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(dirname "$SCRIPT_DIR")"

# Linux portability:
# Detect OS/ARCH to resolve the correct PBS triple. uv venv downloads PBS
# automatically based on the host, but this variable is useful for logs and for
# future steps that need to know the target (cross-build, etc.). It does not modify
# Step 5.5 (PBS copy) — `realpath venv/bin/python` already resolves the real directory
# that uv downloaded, regardless of the platform.
OS=$(uname -s)
ARCH=$(uname -m)
# OS_KIND normalises the platform family. On Windows the build runs under
# Git-for-Windows / MSYS2, where `uname -s` reports MINGW64_NT-* / MSYS_NT-* and
# `uname -m` LIES (reports x86_64 for the x86-emulated bash even on ARM64). We
# force the target arch to aarch64 — what matters is the PBS/wheel target, not
# the shell's arch. Empirically confirmed on the Win11 ARM64 VM (2026-07-01).
case "$OS" in
    Darwin)               OS_KIND=macos ;;
    Linux)                OS_KIND=linux ;;
    MINGW*|MSYS*|CYGWIN*) OS_KIND=windows; ARCH=aarch64 ;;
    *) echo "Unsupported platform: $OS-$ARCH" >&2; exit 1 ;;
esac
case "$OS_KIND-$ARCH" in
    macos-arm64)     PBS_TRIPLE="aarch64-apple-darwin" ;;
    macos-x86_64)    PBS_TRIPLE="x86_64-apple-darwin" ;;
    linux-aarch64)   PBS_TRIPLE="aarch64-unknown-linux-gnu" ;;
    linux-x86_64)    PBS_TRIPLE="x86_64-unknown-linux-gnu" ;;
    windows-aarch64) PBS_TRIPLE="aarch64-pc-windows-msvc" ;;
    *) echo "Unsupported platform: $OS_KIND-$ARCH" >&2; exit 1 ;;
esac
echo "Detected: $OS/$ARCH → OS_KIND=$OS_KIND PBS_TRIPLE=$PBS_TRIPLE"

# ── Config ────────────────────────────────────────────────────────────
PY_VERSION="3.12"
SIDECAR_DIR="${SIDECAR_DIR:-$PROJECT_ROOT/target/sidecar}"
REQUIREMENTS="${REQUIREMENTS:-$SCRIPT_DIR/poc-sidecar/requirements.txt}"
APP_MODULE="${APP_MODULE:-poc-sidecar/app.py}"
# APP_SOURCE_DIR: if set, copies entire directory to app/ (multi-file apps like
# server-nexe). Overrides APP_MODULE. Example:
#   APP_SOURCE_DIR=/path/to/server-nexe REQUIREMENTS=/path/to/requirements.txt \
#   scripts/build-sidecar.sh
APP_SOURCE_DIR="${APP_SOURCE_DIR:-}"

# ── Per-OS venv / interpreter layout ──────────────────────────────────
# POSIX venvs: bin/ + lib/pythonX.Y/site-packages, python under bin/. Windows
# venvs: Scripts/ + Lib/site-packages, python.exe a real copy (NO symlinks), and
# the PBS ships python.exe at its ROOT. These vars replace every previously
# hardcoded venv/bin/python3 / lib/pythonX.Y path so the same script builds on
# all three platforms.
if [ "$OS_KIND" = windows ]; then
    VENV_BIN="Scripts"
    VENV_PY_REL="Scripts/python.exe"
    SITE_PACKAGES_REL="Lib/site-packages"
    PYVENV_HOME_REL="../python-runtime"
else
    VENV_BIN="bin"
    VENV_PY_REL="bin/python3"
    SITE_PACKAGES_REL="lib/python${PY_VERSION}/site-packages"
    PYVENV_HOME_REL="../python-runtime/bin"
fi

# BOOT_PY = the interpreter that BOOTS the sidecar (validate / smoke / boot gates),
# distinct from VENV_PY (used to INSTALL into the venv). On Windows the venv launcher
# (Scripts\python.exe) is a redirector that cannot resolve a relocatable relative
# pyvenv.cfg home once the bundle moves — so we boot the PBS directly
# (python-runtime\python.exe) and expose the venv packages via a relative .pth
# (Step 5.5b). On POSIX BOOT_PY is just the venv python. Empirically validated on
# Win11 ARM64 (2026-07-01). SIDECAR_DIR is already set above (Config).
if [ "$OS_KIND" = windows ]; then
    BOOT_PY="$SIDECAR_DIR/python-runtime/python.exe"
else
    BOOT_PY="$SIDECAR_DIR/venv/$VENV_PY_REL"
fi

# ── Pre-checks ────────────────────────────────────────────────────────
if ! command -v uv &>/dev/null; then
    echo "ERROR: uv not found. Install: curl -LsSf https://astral.sh/uv/install.sh | sh" >&2
    exit 1
fi

echo "==> build-sidecar.sh — PBS + uv packaging POC"
echo "    Python target: $PY_VERSION"
echo "    Output dir:    $SIDECAR_DIR"
echo "    Architecture:  $(uname -m)"
echo ""

# ── Step 1: Clean previous build ─────────────────────────────────────
if [ -d "$SIDECAR_DIR" ]; then
    echo "==> Cleaning previous build..."
    rm -rf "$SIDECAR_DIR"
fi
mkdir -p "$SIDECAR_DIR"

# ── Step 2: Create venv with PBS Python via uv ───────────────────────
# --managed-python: forces uv to use the portable PBS (managed-installations),
# preventing it from reusing a system Python. On Mac uv already downloads PBS because 3.12
# is not a system Python. On Linux (Ubuntu 24.04 ARM64) `/usr/bin/python3.12` exists
# and uv would reuse it → Step 5.5 rsyncs all of /usr → breaks sssd/netplan permissions
# and the bundle is not portable. Empirically validated on the Linux test VM 2026-05-22 evening.
echo "==> Creating venv with Python $PY_VERSION (PBS via uv, managed)..."
START_VENV=$(date +%s)
if [ "$OS_KIND" = windows ]; then
    # uv / python-build-standalone has NO windows-aarch64 PBS (only x86_64), verified via
    # `uv python list --all-platforms`. --managed-python would silently download an x86_64
    # PBS that runs EMULATED under WOW64. For a native ARM64 sidecar we point uv at an
    # explicit ARM64 PBS root (NEXE_WIN_PBS, containing python.exe + Lib\ + DLLs\).
    if [ -z "${NEXE_WIN_PBS:-}" ] || [ ! -f "$NEXE_WIN_PBS/python.exe" ]; then
        echo "ERROR: Windows ARM64 build requires NEXE_WIN_PBS to point at an ARM64" >&2
        echo "       python-build-standalone root (with python.exe). uv has no" >&2
        echo "       windows-aarch64 PBS. Got NEXE_WIN_PBS='${NEXE_WIN_PBS:-<unset>}'." >&2
        exit 1
    fi
    echo "    Using explicit ARM64 PBS: $NEXE_WIN_PBS"
    # uv is a native Windows binary — feed it a Windows path (cygpath -w), while the
    # -f test above uses the unix form ($NEXE_WIN_PBS is /c/nexe/python under git-bash).
    WIN_PBS_PY=$(cygpath -w "$NEXE_WIN_PBS/python.exe" 2>/dev/null || echo "$NEXE_WIN_PBS/python.exe")
    uv venv "$SIDECAR_DIR/venv" --python "$WIN_PBS_PY" --quiet
else
    uv venv "$SIDECAR_DIR/venv" --python "$PY_VERSION" --managed-python --quiet
fi
END_VENV=$(date +%s)
echo "    Venv created in $((END_VENV - START_VENV))s"

# Verify the Python is from PBS (not system)
VENV_PY="$SIDECAR_DIR/venv/$VENV_PY_REL"
PY_PREFIX=$("$VENV_PY" -c "import sys; print(sys.base_prefix)")
echo "    Python prefix: $PY_PREFIX"
echo "    Python version: $("$VENV_PY" --version)"

# ── Step 3: Install dependencies ─────────────────────────────────────
echo "==> Installing dependencies..."
START_DEPS=$(date +%s)
if [ "$OS_KIND" = windows ] && [ -n "${APP_SOURCE_DIR:-}" ]; then
    # ── Windows ARM64 dependency path ─────────────────────────────────
    # Do NOT install requirements.txt as-is: it pulls qdrant-client (grpcio has
    # no win_arm64 wheel) and, via uvicorn[standard], httptools (no wheel). We
    # install a windows-adapted requirements file + qdrant --no-deps + the
    # vendored grpc-shim. Ollama-only inference on the first Windows release
    # (MLX is Apple-only; llama-cpp deferred like the Linux path).
    REQ_WIN="$APP_SOURCE_DIR/requirements-windows.txt"
    if [ ! -f "$REQ_WIN" ]; then
        echo "ERROR: requirements-windows.txt not found at $REQ_WIN (required for win_arm64)" >&2
        exit 1
    fi
    echo "    Installing Windows ARM64 deps from $REQ_WIN..."
    uv pip install --python "$VENV_PY" -r "$REQ_WIN" --quiet
    # qdrant-client WITHOUT deps: grpcio has no win_arm64 wheel and is never used
    # (embedded/local mode, prefer_grpc=False). Its unconditional `import grpc`
    # (core/qdrant_pool.py) is satisfied by the vendored shim installed just below.
    echo "    Installing qdrant-client==1.18.0 (--no-deps) + pure-python runtime deps..."
    uv pip install --python "$VENV_PY" --no-deps "qdrant-client==1.18.0" --quiet
    # qdrant runtime deps (pure-python): portalocker + protobuf + urllib3. urllib3 is
    # imported AT qdrant import time (qdrant_remote.py: `from urllib3.util import ...`) even
    # in embedded/local mode; declare it explicitly rather than relying on fastembed's
    # transitive requests->urllib3 edge (which would break the server if fastembed changes).
    uv pip install --python "$VENV_PY" "portalocker>=2.7.0" "protobuf>=4.21" "urllib3>=1.26.14,<3" --quiet
    # Vendored grpc-shim (2 tiny files, generic dummy metaclass) copied verbatim
    # from the repo into site-packages/grpc so qdrant's import resolves. NOT a real
    # gRPC client — any actual gRPC call path would (correctly) fail.
    GRPC_SHIM_SRC="$APP_SOURCE_DIR/installer/win/grpc_shim"
    if [ ! -d "$GRPC_SHIM_SRC" ]; then
        echo "ERROR: grpc-shim not found at $GRPC_SHIM_SRC (qdrant would fail to import grpc)" >&2
        exit 1
    fi
    echo "    Installing vendored grpc-shim into site-packages/grpc..."
    rm -rf "$SIDECAR_DIR/venv/$SITE_PACKAGES_REL/grpc"
    cp -R "$GRPC_SHIM_SRC" "$SIDECAR_DIR/venv/$SITE_PACKAGES_REL/grpc"
elif [ -f "$REQUIREMENTS" ]; then
    uv pip install --python "$VENV_PY" -r "$REQUIREMENTS" --quiet

    # Platform-specific deps. If APP_SOURCE_DIR is set and contains a
    # requirements-macos.txt (server-nexe pattern: MLX-lm, MLX-vlm, etc.),
    # we install it too. Without this, the MLX + llama_cpp inference engines
    # are not available in the production sidecar and the UI dropdowns only
    # show Ollama. Applicable only to macOS arm64 (host detection).
    if [ "$(uname -s)" = "Darwin" ] && [ "$(uname -m)" = "arm64" ] && [ -n "${APP_SOURCE_DIR:-}" ]; then
        REQ_MACOS="$APP_SOURCE_DIR/requirements-macos.txt"
        if [ -f "$REQ_MACOS" ]; then
            echo "    Installing macOS-specific inference engine deps (MLX, etc.)..."
            uv pip install --python "$VENV_PY" -r "$REQ_MACOS" --quiet
        fi
        # ── MLX backward-compat pin (macOS 14 Sonoma wheels) ─────────────
        # On a macOS 26 (Tahoe) build host, uv resolves the macosx_26 MLX
        # wheels, whose Metal 4.0 metallib FAILS to load on macOS < 26:
        #   [metal::Device] Unable to build metal library from source
        #   error: invalid value 'metal4.0' in '-std=metal4.0'
        # uv cannot target an older macOS deployment via flags (MACOSX_DEPLOYMENT_TARGET
        # is ignored; --python-platform defaults to macOS 13 with no wheel), so we
        # force the macosx_14 (Sonoma, Metal 3.1) wheels by URL. Runs on macOS 14/15/26.
        # The mlx core wheel is cp312 — it MUST match PY_VERSION ($PY_VERSION) above.
        echo "    Pinning MLX to macOS 14 (Sonoma) wheels for backward compatibility..."
        # B136: pin the MLX wheels by URL WITH cryptographic hash verification
        # (defense-in-depth supply-chain). NOTE: the 64-hex segment in the
        # pythonhosted path is blake2b_256, NOT sha256 — these sha256 come from
        # the PyPI JSON API (immutable per file): `curl -sL <url> | shasum -a 256`.
        MLX_WHL="https://files.pythonhosted.org/packages/c3/47/5f33906cb03d6a378a697cd2d2641a26b37dea17ee3d9124d7e39e8eca01/mlx-0.31.2-cp312-cp312-macosx_14_0_arm64.whl"
        MLX_METAL_WHL="https://files.pythonhosted.org/packages/3f/69/fe3b783ebe999f3118234e1e940feb622518bfb1dea6ac5d13b1d36a8449/mlx_metal-0.31.2-py3-none-macosx_14_0_arm64.whl"
        MLX_SHA256="e5067aaf2be1f3d7bba5be52348775804f111173c1ed04639618fd713b1a530f"
        MLX_METAL_SHA256="b25385bcee18fc194092255b8b53b9a3d8489eb650e59160f1b57aadd07aa2dc"
        MLX_REQ="$(mktemp -t mlx-pin.XXXXXX)"
        printf '%s --hash=sha256:%s\n%s --hash=sha256:%s\n' \
            "$MLX_WHL" "$MLX_SHA256" "$MLX_METAL_WHL" "$MLX_METAL_SHA256" > "$MLX_REQ"
        uv pip install --python "$VENV_PY" --reinstall --no-deps --quiet \
            --require-hashes -r "$MLX_REQ"
        rm -f "$MLX_REQ"
        # Fail the build loudly if the pin didn't land (e.g. PY_VERSION bumped → cp tag mismatch).
        MLX_TAG=$(grep -h '^Tag:' "$SIDECAR_DIR"/venv/lib/python*/site-packages/mlx_metal-*.dist-info/WHEEL 2>/dev/null | head -1)
        case "$MLX_TAG" in
            *macosx_14_0*) echo "    MLX pin OK ($MLX_TAG)";;
            *) echo "ERROR: MLX macOS-compat pin failed ($MLX_TAG) — bundle would crash on macOS < 26" >&2; exit 1;;
        esac
        # llama-cpp-python ships its own arm64 wheel with Metal — install it
        # explicitly here (no platform marker in requirements.txt because Linux
        # builds use a different installation path).
        echo "    Installing llama-cpp-python (arm64 macOS wheel with Metal)..."
        uv pip install --python "$VENV_PY" "llama-cpp-python==0.3.19" --quiet
        # torch + torchvision — required at runtime by Qwen3.5 VL family and
        # other multimodal MLX models (Qwen3_5ForConditionalGeneration needs
        # torchvision for image preprocessing even in text-only fallback path).
        # macOS arm64 wheels do NOT include CUDA/cuDNN libs (~92MB net).
        echo "    Installing torch + torchvision (VLM support)..."
        uv pip install --python "$VENV_PY" "torch==2.11.0" "torchvision==0.26.0" --quiet
    elif [ "$(uname -s)" = "Linux" ] && [ -n "${APP_SOURCE_DIR:-}" ]; then
        # First Linux release: Ollama-only. llama-cpp-python in release 1.1.
        # MLX does not apply (Apple-only). If APP_SOURCE_DIR exposes requirements-linux.txt
        # (future server-nexe variant), install the Linux-only extras there;
        # otherwise, the base requirements.txt already includes ollama_module + fastembed.
        REQ_LINUX="$APP_SOURCE_DIR/requirements-linux.txt"
        if [ -f "$REQ_LINUX" ]; then
            echo "    Installing Linux-specific inference engine deps (Ollama-only)..."
            uv pip install --python "$VENV_PY" -r "$REQ_LINUX" --quiet
        else
            echo "    No requirements-linux.txt found at $REQ_LINUX — skipping Linux extras (Ollama-only via base requirements)"
        fi
        # TODO release 1.1 Linux: evaluate the Linux llama-cpp-python wheel (CPU + optional CUDA/Vulkan).
        # Not installed in the 1st release to keep the bundle small + Ollama-only architectural decision.
    fi
else
    # Minimal deps for POC
    uv pip install --python "$VENV_PY" fastapi "uvicorn[standard]" --quiet
fi
END_DEPS=$(date +%s)
echo "    Dependencies installed in $((END_DEPS - START_DEPS))s"

# ── Step 3b: Remove hf_xet ────────────────────────────────────────────
# Belt-and-braces defence against the silent stalled-download bug. The Rust
# launcher already sets HF_HUB_DISABLE_XET=1 before spawning Python, so
# huggingface_hub never enters the xet code path; uninstalling hf_xet at
# build time ensures that even if a future regression drops the env var,
# `is_xet_available()` returns False because the package literally isn't
# importable. hf_xet is an optional dep of huggingface_hub (nothing else in
# the sidecar — fastembed, MLX, llama.cpp, Ollama — relies on it). Empirically
# validated 2026-05-20.
if uv pip list --python "$VENV_PY" 2>/dev/null | grep -qi "^hf-xet\|^hf_xet "; then
    echo "==> Removing hf_xet from bundle (belt-and-braces)..."
    uv pip uninstall --python "$VENV_PY" hf_xet --quiet || true
fi

# ── Step 4: Copy application code ────────────────────────────────────
echo "==> Copying application code..."
mkdir -p "$SIDECAR_DIR/app"
if [ -n "$APP_SOURCE_DIR" ]; then
    # Multi-file mode (server-nexe): copy entire source directory.
    # rsync with excludes to avoid a privacy leak.
    #
    # Root cause — the recurring smoke instability
    # comes from DEV contamination inside the bundle: .test_venv/ test Python
    # venv, node_modules/, scripts/, docs/, README*.md and
    # pytest configs were included in sidecar/app/. The .test_venv in particular
    # exposed .pth files on sys.path that made the module_manager
    # discover plugins in the dev source dir (server-nexe/plugins/) instead
    # of the extracted sidecar. Each fix uncovered a new layer.
    #
    # NOTE: knowledge/ is deliberately NOT on the denylist — it MUST ship in the
    # bundle. It carries the precomputed RAG embeddings (knowledge/.embeddings/*.npz,
    # loaded into Qdrant at first boot) and the source docs the RAG serves. Adding
    # knowledge/ here would silently break RAG (docs never load, no visible error).
    #
    # Leading slash in /pattern = anchored to $APP_SOURCE_DIR; without a slash it
    # matches at any depth (which is why, without a slash, it also excludes
    # memory/memory/storage/, a real Python module that we do NOT want excluded).
    if [ "$OS_KIND" = windows ]; then
        # No rsync in Git-for-Windows. Copy the whole tree then prune the same
        # denylist rsync applies below (tar --exclude glob semantics differ across
        # GNU/bsdtar; explicit copy+prune is predictable). verify-privacy-gate.sh is the net.
        cp -R "$APP_SOURCE_DIR/." "$SIDECAR_DIR/app/"
        ( cd "$SIDECAR_DIR/app" && rm -rf \
            storage .env .git diari tests InstallNexe.app Nexe.app .internal-audit dev-tools \
            .test_venv .venv .test_data worktrees .github .grimp_cache .ruff_cache \
            node_modules .pytest_cache .mypy_cache .coverage docs specialists scripts \
            SetupNexe.command setup.sh nexe eslint.config.js package.json package-lock.json \
            pytest.ini pytest-full.ini conftest.py .module_cache.json \
            installer/swift-wizard installer/NexeTray.app installer/tray_icons \
            installer/build_dmg.sh installer/build-embedding-bundle.sh \
            installer/build-ollama-bundle.sh installer/build-python-bundle.sh \
            installer/build-wheels-bundle.sh installer/sign-wheels-bundle.sh \
            installer/install.py installer/install_headless.py installer/tray.py \
            installer/tray_monitor.py installer/tray_translations.py installer/tray_uninstaller.py \
            installer/nexe_launcher.swift installer/make_dmg_ds_store.py \
            installer/dmg_background.png installer/logo.png \
            installer/ollama-checksums.txt installer/wheels-checksums.txt 2>/dev/null || true
          rm -rf README*.md CHANGELOG.md LICENSE SECURITY.md THREAT_MODEL.md \
            CODE_OF_CONDUCT.md CONTRIBUTING.md COMMANDS.md index_server-nexe.md 2>/dev/null || true
          find . -depth -type d -name '__pycache__' -exec rm -rf {} + 2>/dev/null || true
          find . -type d -name '*.egg-info' -exec rm -rf {} + 2>/dev/null || true
          find . -name '.DS_Store' -delete 2>/dev/null || true
          find . -name '._*' -delete 2>/dev/null || true )
        echo "    Windows copy+prune done (denylist parity with rsync)"
    else
    rsync -a \
        --exclude='/storage' --exclude='.env' --exclude='/.git' \
        --exclude='__pycache__' --exclude='/venv' --exclude='/diari' \
        --exclude='/tests' --exclude='/InstallNexe.app' --exclude='/Nexe.app' \
        --exclude='/.internal-audit' --exclude='/dev-tools' \
        --exclude='/.test_venv' --exclude='/.venv' \
        --exclude='/.test_data' --exclude='/worktrees' \
        --exclude='/.github' --exclude='/.grimp_cache' --exclude='/.ruff_cache' \
        --exclude='/node_modules' --exclude='/.pytest_cache' --exclude='/.mypy_cache' \
        --exclude='/.coverage' --exclude='.DS_Store' --exclude='._*' \
        --exclude='/docs' --exclude='/specialists' \
        --exclude='/scripts' --exclude='/SetupNexe.command' --exclude='/setup.sh' \
        --exclude='/nexe' \
        --exclude='/eslint.config.js' --exclude='/package.json' --exclude='/package-lock.json' \
        --exclude='/pytest.ini' --exclude='/pytest-full.ini' --exclude='/conftest.py' \
        --exclude='*.egg-info' \
        --exclude='/README*.md' --exclude='/CHANGELOG.md' --exclude='/LICENSE' \
        --exclude='/SECURITY.md' --exclude='/THREAT_MODEL.md' \
        --exclude='/CODE_OF_CONDUCT.md' --exclude='/CONTRIBUTING.md' \
        --exclude='/COMMANDS.md' --exclude='/index_server-nexe.md' \
        --exclude='.module_cache.json' \
        --exclude='/installer/swift-wizard' \
        --exclude='/installer/NexeTray.app' \
        --exclude='/installer/tray_icons' \
        --exclude='/installer/build_dmg.sh' \
        --exclude='/installer/build-embedding-bundle.sh' \
        --exclude='/installer/build-ollama-bundle.sh' \
        --exclude='/installer/build-python-bundle.sh' \
        --exclude='/installer/build-wheels-bundle.sh' \
        --exclude='/installer/sign-wheels-bundle.sh' \
        --exclude='/installer/install.py' \
        --exclude='/installer/install_headless.py' \
        --exclude='/installer/tray.py' \
        --exclude='/installer/tray_monitor.py' \
        --exclude='/installer/tray_translations.py' \
        --exclude='/installer/tray_uninstaller.py' \
        --exclude='/installer/nexe_launcher.swift' \
        --exclude='/installer/make_dmg_ds_store.py' \
        --exclude='/installer/dmg_background.png' \
        --exclude='/installer/logo.png' \
        --exclude='/installer/ollama-checksums.txt' \
        --exclude='/installer/wheels-checksums.txt' \
        "$APP_SOURCE_DIR/." "$SIDECAR_DIR/app/"
    fi
    # We include the installer/ Python modules that the sidecar
    # imports at runtime — installer_ollama_install (ensure_ollama_installed),
    # download_verify (verify_download_integrity), installer_catalog_data
    # (MODEL_WEIGHT_SHA256), installer_hardware, installer_setup_env (preseed),
    # installer_setup_models (download pattern).
    #
    # IMPORTANT: the dependency graph forces us to keep the WHOLE installer_*.py family
    # because ollama_install and everything else import .installer_display + .installer_i18n,
    # and installer_i18n imports .installer_translations*. Excluding any of them breaks
    # the import chain. They weigh a few KB (terminal print + strings), neutral.
    #
    # We exclude swift-wizard (278 MB, notarytool log 4d42c92d), legacy NexeTray.app,
    # tray_*.py and wheels-checksums.txt (legacy CLI), build_*.sh scripts, DMG images
    # and the standalone CLI (install.py + install_headless.py). Everything has an
    # equivalent in nexe-app/Tauri (wizard HTML + native tray + its own scripts).
    echo "    Source dir: $APP_SOURCE_DIR"
else
    # Single-file mode (poc-sidecar default): copy one .py file as app.py.
    cp "$SCRIPT_DIR/$APP_MODULE" "$SIDECAR_DIR/app/app.py"
fi

# ── Step 4.5: Pre-seed fastembed embedder cache ───────────────────────
# Without this preseed, the first chat after the wizard would silently fail
# because fastembed tried to download the paraphrase-multilingual-
# mpnet-base-v2 model on the first TextEmbedding() call. With HF_HUB_OFFLINE=1 forced
# by the lifespan, the download would break.
#
# Strategy (empirically validated 2026-05-20):
# - Pre-seed during build into app/.fastembed_cache/ (staging inside the bundle).
# - On the sidecar's first launch (Step 5.9 of the launcher), copy from the bundle to
#   ~/.cache/fastembed/ (writable) — see the original `_seed_fastembed_cache()`
#   at installer/installer_setup_env.py:207 for the equivalent logic.
# - Do NOT set FASTEMBED_CACHE_DIR in the launcher: fastembed writes
#   `files_metadata.json` on first load → would cause a PermissionError when read-only.
#
# Graceful: if the build host has no internet, warn and continue. The download
# will happen online at the first chat (as before, but with clear logs).
if [ -n "${APP_SOURCE_DIR:-}" ]; then
    echo "==> Pre-seeding fastembed embedder cache..."
    START_FE=$(date +%s)
    FASTEMBED_STAGING="$SIDECAR_DIR/app/.fastembed_cache"
    mkdir -p "$FASTEMBED_STAGING"
    # IMPORTANT 1: the fastembed library does NOT honour the FASTEMBED_CACHE_DIR env var
    # (empirically verified 2026-05-20). The cache_dir must be passed explicitly to the
    # TextEmbedding(model, cache_dir=...) constructor.
    # IMPORTANT 2: Step 4.5 runs BEFORE the PBS copy (Step 5.5), so
    # python-runtime/ does NOT yet exist in the bundle. We do NOT set PYTHONHOME —
    # we let the venv use its natural PBS (uv default).
    PYTHONNOUSERSITE=1 \
      FASTEMBED_STAGING_PATH="$FASTEMBED_STAGING" \
      "$VENV_PY" -c "import os; from fastembed import TextEmbedding; TextEmbedding('sentence-transformers/paraphrase-multilingual-mpnet-base-v2', cache_dir=os.environ['FASTEMBED_STAGING_PATH'])" \
      || echo "    WARN: fastembed preseed failed (offline build?) — model will be downloaded at first chat"
    END_FE=$(date +%s)
    if [ -d "$FASTEMBED_STAGING" ] && [ -n "$(ls -A "$FASTEMBED_STAGING" 2>/dev/null)" ]; then
        FE_SIZE=$(du -sh "$FASTEMBED_STAGING" | cut -f1)
        echo "    Pre-seed completed in $((END_FE - START_FE))s ($FE_SIZE)"
    else
        echo "    Pre-seed dir empty (offline build) — bundle ships without embedder cache"
    fi
fi

# ── Step 5: Create launcher script (POSIX only) ──────────────────────
# On Windows the runtime spawns venv\Scripts\python.exe -m uvicorn directly
# (lib.rs build_windows_sidecar_command) — no bash launcher is generated.
if [ "$OS_KIND" != windows ]; then
cat > "$SIDECAR_DIR/nexe-sidecar" << 'LAUNCHER'
#!/bin/bash
# Launcher for nexe-sidecar — self-contained Python server
# This script is what Tauri's externalBin / Command would invoke.
set -euo pipefail

SIDECAR_DIR="${NEXE_SIDECAR_DIR:-$(cd "$(dirname "$0")" && pwd)}"
VENV_PY="$SIDECAR_DIR/venv/bin/python3"

# Ensure no system Python contamination
export PYTHONNOUSERSITE=1
export PYTHONDONTWRITEBYTECODE=1
# PBS portable safety net. If pyvenv.cfg `home=relative` fails for some
# reason (Python rejects the path, future build with a different PBS structure),
# an explicit PYTHONHOME guarantees sys.base_prefix points to the PBS inside the bundle.
export PYTHONHOME="$SIDECAR_DIR/python-runtime"
# Unbuffered I/O: emit logs in real time (no stdout/stderr buffering).
# Required so Rust spawner can capture sidecar logs as they happen,
# especially during early-fail scenarios before /health/ready binds.
export PYTHONUNBUFFERED=1

# Seed fastembed cache to ~/.cache/fastembed/ at
# first launch. The bundle ships the cache pre-seeded at app/.fastembed_cache/
# (read-only inside the signed app). fastembed writes files_metadata.json on
# first load → if the cache were read-only, PermissionError. Solution:
# copy to the user cache (writable) only on first launch. Reproduces the
# logic of installer/installer_setup_env.py:_seed_fastembed_cache().
# Empirically validated 2026-05-20 (Option B).
EMBEDDER_DIR="$HOME/.cache/fastembed/models--sentence-transformers--paraphrase-multilingual-mpnet-base-v2"
if [ -d "$SIDECAR_DIR/app/.fastembed_cache" ] && [ ! -d "$EMBEDDER_DIR" ]; then
    echo "First launch: seeding fastembed cache to ~/.cache/fastembed/..." >&2
    mkdir -p "$HOME/.cache/fastembed"
    cp -R "$SIDECAR_DIR/app/.fastembed_cache/." "$HOME/.cache/fastembed/" 2>/dev/null || \
        echo "WARN: fastembed seed failed (will download at first chat)" >&2
fi

# Read auth token from stdin (NOT env var) so it never appears in
# /proc/<pid>/environ nor in `ps eww` output. The Rust spawner (lib.rs setup)
# writes "<token>\n" to stdin then closes the pipe.
#
# We read the first line, store it in a local shell var, and pass it to the
# Python child via a non-deterministic env var name + scrub the obvious
# `NEXE_AUTH_TOKEN` slot if anything inherited it. The Python sidecar reads
# `NEXE_TOKEN_INTERNAL`. (Yes this still surfaces in environ, but with a
# different name and we've removed the well-known leak vector.)
#
# Future hardening: pass via dup'd FD (read directly into Python without ever
# touching env). For now stdin-then-export is the pragmatic mid.
read -r -t 5 NEXE_TOKEN_VALUE || { echo "ERROR: stdin token read timeout" >&2; exit 1; }
unset NEXE_AUTH_TOKEN  # belt + suspenders if Rust spawner ever sets both
export NEXE_TOKEN_INTERNAL="$NEXE_TOKEN_VALUE"
unset NEXE_TOKEN_VALUE

HOST="${NEXE_HOST:-127.0.0.1}"
# Default port unified with lib.rs SIDECAR_PORT (single source of truth);
# Tauri spawn passes NEXE_PORT=8765 explicitly.
PORT="${NEXE_PORT:-8765}"

exec "$VENV_PY" -m uvicorn core.app:app \
    --host "$HOST" \
    --port "$PORT" \
    --workers 1 --lifespan on \
    --no-access-log \
    --app-dir "$SIDECAR_DIR/app"
LAUNCHER
chmod +x "$SIDECAR_DIR/nexe-sidecar"
else
    echo "==> Step 5 (bash launcher) skipped on Windows — runtime uses direct python.exe spawn"
fi

# ── Step 5.5: Copy Python Build Standalone into bundle ──────────────
# Root bug discovered 2026-05-18: `uv venv` creates absolute symlinks to the
# build user's PBS (~/.local/share/uv/python/cpython-3.12.11-.../bin/
# python3.12). On the target Mac (different user), the symlink is broken and
# launcher line 39 fails with "No such file or directory". Solution: copy
# the entire PBS into the bundle, make the symlinks relative, make pyvenv.cfg
# relocatable. Empirically validated 2026-05-18.
echo "==> Copying PBS runtime into bundle (portable)..."
# PBS root resolved from the interpreter itself (Step 2 already computed base_prefix
# as PY_PREFIX). Platform-agnostic: on Windows uv creates no venv symlink for realpath
# to follow, and sys.base_prefix is the PBS root on all platforms.
PBS_DIR="$PY_PREFIX"
echo "    PBS source: $PBS_DIR"
mkdir -p "$SIDECAR_DIR/python-runtime"
if [ "$OS_KIND" = windows ]; then
    # No rsync in Git-for-Windows. tar copy-through (PBS Windows ships no symlinks).
    # Exclude include/ (headers) + share/ (docs) like the POSIX path.
    tar -C "$PBS_DIR" --exclude='./include' --exclude='./share' -cf - . \
        | tar -C "$SIDECAR_DIR/python-runtime" -xf -
else
    # rsync -a preserves intra-PBS symlinks and permissions. Excludes include/ (~8 MB
    # headers for compilation, not needed at runtime) and share/ (~1 MB doc).
    rsync -a --delete \
        --exclude='include/' \
        --exclude='share/' \
        "$PBS_DIR/" "$SIDECAR_DIR/python-runtime/"
fi
PBS_SIZE=$(du -sh "$SIDECAR_DIR/python-runtime" | cut -f1)
echo "    PBS copied: $PBS_SIZE"

# ── Step 5.5b: link PBS -> venv site-packages via a relative .pth (Windows) ──
# The venv launcher can't resolve a relocatable relative home, so the runtime boots
# the PBS directly. A RELATIVE .pth in the PBS site-packages exposes the venv's
# installed packages to the PBS, resolving wherever the bundle is extracted:
# from python-runtime\Lib\site-packages, `..\..\..\venv\Lib\site-packages` climbs to
# the bundle root then into the venv. Portable (no absolute path). Validated 2026-07-01.
if [ "$OS_KIND" = windows ]; then
    # Use an `import ...; addsitedir(...)` line (NOT a bare path) so the venv's OWN .pth
    # files are also processed — pywin32 ships a bootstrap .pth that registers its DLLs
    # (pywintypes/pythoncom), which portalocker's Win32Locker (qdrant local) needs. A bare
    # path line only appends to sys.path and skips those. sys.prefix is the PBS root
    # (python-runtime); ..\venv\Lib\site-packages resolves portably at any extract location.
    printf "import site, os, sys; site.addsitedir(os.path.join(sys.prefix, '..', 'venv', 'Lib', 'site-packages'))\r\n" \
        > "$SIDECAR_DIR/python-runtime/$SITE_PACKAGES_REL/nexe_venv.pth"
    echo "    Wrote addsitedir .pth: PBS -> ..\\venv\\Lib\\site-packages (processes venv .pth too)"
fi

# ── Step 5.6: Rewrite venv symlinks relatively ──────────────────────
# The 3 venv/bin/ symlinks (python, python3, python3.12) now point to the
# absolute PBS of the build machine. They must be replaced with RELATIVE symlinks to
# the python-runtime/ we just copied. This way, when Tauri extracts the tarball to
# ~/Library/Application Support/com.nexe.app/sidecar/, the symlinks resolve
# correctly inside the extracted directory.
echo "==> Rewriting venv symlinks to relative PBS paths..."
if [ "$OS_KIND" != windows ]; then
( cd "$SIDECAR_DIR/venv/bin" && \
    rm -f python python3 python3.12 && \
    ln -sf ../../python-runtime/bin/python3.12 python3.12 && \
    ln -sf ../../python-runtime/bin/python3.12 python3 && \
    ln -sf python3 python )
echo "    Symlinks: python, python3, python3.12 -> ../../python-runtime/bin/python3.12"
else
    echo "    Step 5.6 skipped on Windows (venv\\Scripts\\python.exe is a real copy; bundle is symlink-free)"
fi

# ── Step 5.7: Rewrite pyvenv.cfg relocatable ────────────────────────
# Replaces `home = $HOME/.local/share/uv/python/.../bin` with a relative
# path (../../python-runtime/bin) and enables `relocatable = true`. Python 3.12
# resolves a relative `home` against the pyvenv.cfg directory (venv/), via site.py.
# `relocatable = true` forces sys.prefix to be recomputed from the venv's real
# location (not from a hardcoded absolute `home`), covering the case where `home` fails
# to resolve. Combined with PYTHONHOME in the launcher (Step 5), it is the most
# robust configuration (source: CPython Lib/site.py).
echo "==> Rewriting pyvenv.cfg for relocatable PBS..."
UV_VERSION_STR=$(uv --version 2>/dev/null | awk '{print $2}')
PY_FULL_VERSION=$(PYTHONHOME="$SIDECAR_DIR/python-runtime" "$BOOT_PY" -c "import platform; print(platform.python_version())" 2>/dev/null || echo "3.12.0")
# home= is RELATIVE to the pyvenv.cfg directory (venv/). POSIX PBS keeps python under
# bin/ (home=../python-runtime/bin); Windows PBS ships python.exe at its ROOT
# (home=../python-runtime). $PYVENV_HOME_REL is set per-OS at the top. version_info is
# derived from the interpreter (was hardcoded 3.12.11, brittle on a PY_VERSION bump).
cat > "$SIDECAR_DIR/venv/pyvenv.cfg" <<PYVENV
home = $PYVENV_HOME_REL
implementation = CPython
uv = ${UV_VERSION_STR}
version_info = ${PY_FULL_VERSION}
include-system-site-packages = false
relocatable = true
PYVENV
echo "    pyvenv.cfg rewritten (home=$PYVENV_HOME_REL, version=$PY_FULL_VERSION)"

# ── Step 5.8: Portability verification (Gates G1-G6) ─────────────────
echo "==> Portability verification..."

# G1: no absolute symlinks anywhere in the bundle
ABS_SYMLINKS=$(find "$SIDECAR_DIR" -type l -lname '/*' 2>/dev/null | wc -l | tr -d ' ')
if [ "$ABS_SYMLINKS" -ne 0 ]; then
    echo "    G1 FAIL: $ABS_SYMLINKS absolute symlinks found:"
    find "$SIDECAR_DIR" -type l -lname '/*' 2>/dev/null
    exit 1
fi
echo "    G1 PASS: no absolute symlinks"

# G2: venv/bin/python3 resolves to a real Mach-O (macOS) / ELF (Linux).
# Cross-platform gate via $OS detected at top. NOTE: $OS is HOST OS; for future
# cross-build (Mac → Linux target), introduce $TARGET_OS explicitly.
VENV_PY_LINK="$SIDECAR_DIR/venv/$VENV_PY_REL"
if ! command -v file &>/dev/null && [ "$OS_KIND" != windows ]; then
    echo "    G2 FAIL: 'file' command not found (install: apt-get install -y file / brew install file)"
    exit 1
fi
case "$OS_KIND" in
    macos)
        PY_TYPE=$(file -L "$VENV_PY_LINK" 2>/dev/null)
        if ! echo "$PY_TYPE" | grep -q "Mach-O 64-bit executable arm64"; then
            echo "    G2 FAIL: venv python is not a Mach-O arm64 executable"
            echo "    file: $PY_TYPE"
            exit 1
        fi
        echo "    G2 PASS: venv python resolves to Mach-O arm64"
        ;;
    linux)
        PY_TYPE=$(file -L "$VENV_PY_LINK" 2>/dev/null)
        if ! echo "$PY_TYPE" | grep -qE "ELF 64-bit LSB.*executable.*(ARM aarch64|x86-64)"; then
            echo "    G2 FAIL: venv python is not an ELF 64-bit aarch64/x86_64 executable"
            echo "    file: $PY_TYPE"
            exit 1
        fi
        echo "    G2 PASS: venv python resolves to ELF 64-bit ($ARCH)"
        ;;
    windows)
        # Validate a real PE executable via the 'MZ' magic (first two bytes), without
        # relying on `file` output strings. venv\Scripts\python.exe must be a real copy.
        PY_MAGIC=$(head -c 2 "$VENV_PY_LINK" 2>/dev/null)
        if [ "$PY_MAGIC" != "MZ" ]; then
            echo "    G2 FAIL: venv Scripts/python.exe is not a PE executable (magic='$PY_MAGIC')"
            exit 1
        fi
        echo "    G2 PASS: venv Scripts/python.exe is a PE executable (MZ)"
        ;;
esac

# G3: sys.executable resolves to a path inside the bundle.
# Simulates the real launcher: PYTHONHOME points to the PBS inside the bundle. Without
# PYTHONHOME, uv's PBS has a hardcoded prefix (/install) that fails. The
# launcher ALWAYS defines it (Step 5), which is why the test does too.
SYS_EXEC=$(PYTHONHOME="$SIDECAR_DIR/python-runtime" "$BOOT_PY" -c "import sys; print(sys.executable)")
if [ "$OS_KIND" = windows ]; then
    # Windows python prints C:\...\python.exe (backslashes); normalise both sides to
    # forward-slash lowercase before the prefix check (cygpath handles the drive form).
    SYS_EXEC_N=$(cygpath -u "$SYS_EXEC" 2>/dev/null | tr 'A-Z' 'a-z')
    SIDECAR_N=$(cygpath -u "$SIDECAR_DIR" 2>/dev/null | tr 'A-Z' 'a-z')
else
    SYS_EXEC_N="$SYS_EXEC"; SIDECAR_N="$SIDECAR_DIR"
fi
case "$SYS_EXEC_N" in
    "$SIDECAR_N"/*)
        echo "    G3 PASS: sys.executable inside bundle: $SYS_EXEC"
        ;;
    *)
        echo "    G3 FAIL: sys.executable points outside bundle: $SYS_EXEC"
        exit 1
        ;;
esac

# G5: portability test - copy sidecar to /tmp/ and verify python3 still works.
# Redefine PYTHONHOME pointing to the python-runtime/ of the COPY (simulating
# what the launcher would do on the target Mac, where PYTHONHOME is derived
# dynamically from $SIDECAR_DIR/python-runtime).
# Linux: minimal copy (~50 MB) for space-constrained builders (the Linux test VM UTM
# may have <500 MB free before the resize). Mac: historical full copy (~400 MB).
PORT_TEST_DIR="${TMPDIR:-/tmp}/nexe-sidecar-portable-test-$$"
echo "==> G5 portability test (copy to $PORT_TEST_DIR)..."
rm -rf "$PORT_TEST_DIR"
mkdir -p "$PORT_TEST_DIR"
case "$OS_KIND" in
    macos)
        cp -R "$SIDECAR_DIR/." "$PORT_TEST_DIR/"
        ;;
    linux)
        # Enough to validate that the copied Python boots + stdlib C ext OK.
        mkdir -p "$PORT_TEST_DIR/python-runtime" "$PORT_TEST_DIR/venv"
        cp -R "$SIDECAR_DIR/python-runtime/." "$PORT_TEST_DIR/python-runtime/"
        cp -R "$SIDECAR_DIR/venv/bin" "$PORT_TEST_DIR/venv/"
        ;;
    windows)
        # Copy only the PBS (python-runtime) — the portable boot interpreter. Its relative
        # .pth to the venv finds nothing in this copy (no venv here), which is fine: G5
        # validates PBS + stdlib portability; venv deps are covered by the smoke test.
        mkdir -p "$PORT_TEST_DIR/python-runtime"
        cp -R "$SIDECAR_DIR/python-runtime/." "$PORT_TEST_DIR/python-runtime/"
        ;;
esac
if [ "$OS_KIND" = windows ]; then
    PORT_TEST_PY="$PORT_TEST_DIR/python-runtime/python.exe"
else
    PORT_TEST_PY="$PORT_TEST_DIR/venv/$VENV_PY_REL"
fi
if ! PYTHONHOME="$PORT_TEST_DIR/python-runtime" "$PORT_TEST_PY" --version >/dev/null 2>&1; then
    echo "    G5 FAIL: python from copied bundle does not run"
    PYTHONHOME="$PORT_TEST_DIR/python-runtime" "$PORT_TEST_PY" --version 2>&1 || true
    rm -rf "$PORT_TEST_DIR"
    exit 1
fi
if ! PYTHONHOME="$PORT_TEST_DIR/python-runtime" "$PORT_TEST_PY" -c "import ssl, socket, hashlib" >/dev/null 2>&1; then
    echo "    G5 FAIL: stdlib C extensions not importable from copied bundle"
    PYTHONHOME="$PORT_TEST_DIR/python-runtime" "$PORT_TEST_PY" -c "import ssl, socket, hashlib" 2>&1 || true
    rm -rf "$PORT_TEST_DIR"
    exit 1
fi
rm -rf "$PORT_TEST_DIR"
echo "    G5 PASS: copied bundle python works + stdlib C extensions OK"

# G6: no builder home references in TEXT content of the bundle.
# Tolerates matches in egg-info/RECORD (harmless metadata, no runtime impact).
# Linux: $HOME covers /root, LDAP (/export/home/...), NixOS, Docker (/app),
# WSL and other custom homes. Mac: /Users/$BUILDER preserves the historical
# behaviour (on macOS $HOME = /Users/$USER always, semantically equivalent).
BUILDER=$(whoami)
case "$OS_KIND" in
    macos)   BUILDER_HOME_PREFIX="/Users/$BUILDER" ;;
    linux)   BUILDER_HOME_PREFIX="$HOME" ;;
    windows) BUILDER_HOME_PREFIX="$HOME" ;;   # git-bash $HOME = /c/Users/<user>
    *)       BUILDER_HOME_PREFIX="${HOME:-/nonexistent-home}" ;;
esac
# `|| true`: a non-matching grep returns 1, which under pipefail+set -e aborts the whole
# build (silently, right after G5) — this fires on Windows where the bundle embeds native
# C:\Users paths, not the unix $HOME (/c/Users). Also exclude the fastembed cache (~1.1 GB
# of binary blobs) so the recursive grep stays fast.
GREP_HITS=$( { grep -rI "$BUILDER_HOME_PREFIX" "$SIDECAR_DIR" --exclude-dir='.fastembed_cache' 2>/dev/null || true; } | wc -l | tr -d ' ')
if [ "$GREP_HITS" -ne 0 ]; then
    echo "    G6 WARN: $GREP_HITS references to $BUILDER_HOME_PREFIX found (first 5):"
    # `|| true` neutralizes exit 141 (SIGPIPE) that fires when head -5 closes
    # the pipe before grep finishes. set -e pipefail (line 10) does not forgive 141,
    # it aborts the script. On Mac with few hits it went unnoticed; on Linux with 26+
    # refs (venv activate scripts) the SIGPIPE is guaranteed.
    grep -rI "$BUILDER_HOME_PREFIX" "$SIDECAR_DIR" 2>/dev/null | head -5 || true
else
    echo "    G6 PASS: no $BUILDER_HOME_PREFIX references in text files"
fi

# ── Step 6: Trim unnecessary files ───────────────────────────────────
echo "==> Trimming unnecessary files..."
TRIMMED=0
# Remove __pycache__
FOUND=$(find "$SIDECAR_DIR/venv" -type d -name "__pycache__" | wc -l | tr -d ' ')
find "$SIDECAR_DIR/venv" -type d -name "__pycache__" -exec rm -rf {} + 2>/dev/null || true
TRIMMED=$((TRIMMED + FOUND))

# Remove pip/setuptools cache
rm -rf "$SIDECAR_DIR/venv/$SITE_PACKAGES_REL/pip" 2>/dev/null || true
rm -rf "$SIDECAR_DIR/venv/$SITE_PACKAGES_REL/setuptools" 2>/dev/null || true

# Remove test directories inside site-packages
find "$SIDECAR_DIR/venv/$SITE_PACKAGES_REL" -type d -name "tests" -exec rm -rf {} + 2>/dev/null || true
find "$SIDECAR_DIR/venv/$SITE_PACKAGES_REL" -type d -name "test" -exec rm -rf {} + 2>/dev/null || true

# B135: remove venv activate scripts — they embed the builder's absolute
# $VIRTUAL_ENV path (a plaintext home-dir leak that travels into the DMG).
# The launcher NEVER sources them (it runs "$VENV_PY" -m uvicorn with PYTHONHOME),
# so dropping them is safe. A non-matching glob stays literal and `rm -f` returns 0
# under set -euo pipefail. (Text-only mitigation: binaries still embed the path
# via install-names/.pyc co_filename — see G6, which stays WARN by design.)
rm -f "$SIDECAR_DIR"/venv/"$VENV_BIN"/activate* "$SIDECAR_DIR"/venv/"$VENV_BIN"/Activate* "$SIDECAR_DIR"/venv/"$VENV_BIN"/deactivate* 2>/dev/null || true

echo "    Trimmed $TRIMMED __pycache__ dirs + pip/setuptools/test dirs"

# ── Step 6.5: Sign Mach-O binaries in venv ──────────────────────────
# For Apple notarization, ALL .so/.dylib in the venv must carry
# Developer ID + secure timestamp + hardened runtime. If APPLE_SIGNING_IDENTITY
# is set, it signs ~330 binaries (~1-3 min). Without an identity, it skips with a warning
# (local dev build without a cert). The subsequent smoke test validates that the
# signed binaries still import correctly.
if [ "$OS_KIND" = macos ] && [ -n "${APPLE_SIGNING_IDENTITY:-}" ]; then
    bash "$SCRIPT_DIR/sign-sidecar-binaries.sh" "$SIDECAR_DIR"
elif [ "$OS_KIND" = macos ]; then
    echo "==> Sign step skipped (APPLE_SIGNING_IDENTITY unset — dev build)"
elif [ "$OS_KIND" = windows ] && [ -n "${WINDOWS_SIGNING_CERT:-}" ]; then
    # Authenticode signing of *.exe/*.dll/*.pyd via signtool (PowerShell). Gated by
    # WINDOWS_SIGNING_CERT (thumbprint or /f path). Deferred by default (unsigned build).
    powershell -NoProfile -ExecutionPolicy Bypass -File "$SCRIPT_DIR/sign-sidecar-binaries.ps1" "$SIDECAR_DIR" \
        || { echo "ERROR: Windows sidecar signing failed" >&2; exit 1; }
elif [ "$OS_KIND" = windows ]; then
    echo "==> Sign step skipped (WINDOWS_SIGNING_CERT unset — unsigned dev/validation build)"
else
    echo "==> Sign step skipped ($OS_KIND — codesign no aplica)"
fi

# ── Step 6b: Copy launcher to src-tauri/binaries/ for Tauri externalBin ─
# Tauri 2 externalBin expects: src-tauri/binaries/<name>-<host-triple>
echo "==> Copying launcher to src-tauri/binaries/ for Tauri externalBin..."
HOST_TRIPLE="$(rustc -vV 2>/dev/null | grep '^host:' | awk '{print $2}')"
if [ -z "$HOST_TRIPLE" ]; then
    echo "    WARNING: rustc not found, skipping externalBin copy step"
else
    BINARIES_DIR="$PROJECT_ROOT/src-tauri/binaries"
    mkdir -p "$BINARIES_DIR"
    if [ "$OS_KIND" = windows ]; then
        # Tauri requires the externalBin to carry the .exe suffix on Windows. The runtime
        # NEVER executes it (it spawns python-runtime\python.exe from the extracted data-dir
        # via build_windows_sidecar_command); it only needs to exist as a valid PE to satisfy
        # Tauri's bundler + the resolve_sidecar_path_prod gate. Copy the bundled PBS
        # python.exe (a real ARM64 PE) as an inert stub — avoids needing a Rust/MSVC linker.
        STUB="$BINARIES_DIR/nexe-sidecar-$HOST_TRIPLE.exe"
        cp "$SIDECAR_DIR/python-runtime/python.exe" "$STUB"
        echo "    Copied inert PE externalBin stub (bundled python.exe): $STUB"
    else
        cp "$SIDECAR_DIR/nexe-sidecar" "$BINARIES_DIR/nexe-sidecar-$HOST_TRIPLE"
        chmod +x "$BINARIES_DIR/nexe-sidecar-$HOST_TRIPLE"
        echo "    Copied to: $BINARIES_DIR/nexe-sidecar-$HOST_TRIPLE"
    fi
fi

# ── Step 7: Validate ─────────────────────────────────────────────────
# uv's PBS has a hardcoded /install prefix — PYTHONHOME is MANDATORY to
# find the `encodings` module at bootstrap (init_fs_encoding). The pyvenv.cfg
# `home` only applies post-bootstrap (venv site-packages discovery).
# Empirically validated 2026-05-18 build run 1: without PYTHONHOME it fails with
# "Fatal Python error: init_fs_encoding: failed... No module named 'encodings'".
# The launcher (Step 5) always defines PYTHONHOME, just like the tests here.
echo "==> Validating sidecar..."
PYTHONHOME="$SIDECAR_DIR/python-runtime" "$BOOT_PY" -c "import fastapi; print(f'  FastAPI {fastapi.__version__}')"
PYTHONHOME="$SIDECAR_DIR/python-runtime" "$BOOT_PY" -c "import uvicorn; print(f'  uvicorn {uvicorn.__version__}')"

# Authenticated smoke test: generate a real UUID token + dynamic port so the
# test mirrors the actual Tauri spawn contract (token via stdin, port via env).
# An empty token would cause app.py to call os._exit(1) immediately.
echo "==> Smoke test (boot + authenticated health check)..."
SMOKE_TOKEN=$(PYTHONHOME="$SIDECAR_DIR/python-runtime" "$BOOT_PY" -c "import uuid; print(uuid.uuid4())")
SMOKE_PORT=$(PYTHONHOME="$SIDECAR_DIR/python-runtime" "$BOOT_PY" -c "import socket; s=socket.socket(); s.bind(('127.0.0.1',0)); p=s.getsockname()[1]; s.close(); print(p)")
# B137: PID-suffixed boot log (consistency with PORT_TEST_DIR which already uses $$).
# Used in 4 places below INCLUDING the verify-encryption-gate.sh call (CRY-01) —
# all must reference this var or the gate reads a stale/missing path.
SMOKE_BOOT_LOG="/tmp/nexe-sidecar-boot-$$.log"

# Health endpoint depends on the sidecar app.
# - POC default (poc-sidecar/app.py): /api/v1/system/health
# - real server-nexe: /admin/system/health (registered at system.py:246)
# Detection via APP_SOURCE_DIR (empty=POC, set=multi-file → we assume server-nexe).
# When the endpoint unification is refactored, this conditional disappears.
if [ -n "$APP_SOURCE_DIR" ]; then
    HEALTH_PATH="/admin/system/health"
    # server-nexe boots slowly (RAG + memory + tray + fastembed pre-warm)
    SMOKE_BOOT_MAX_WAIT=30
else
    HEALTH_PATH="/api/v1/system/health"
    SMOKE_BOOT_MAX_WAIT=5
fi

# Env vars that Tauri (lib.rs spawn_sidecar_process) injects in production.
# We replicate them here in the smoke test for consistency — without this, validate_production_security
# (factory_security.py) raises ValueError before reaching uvicorn.
if [ "$OS_KIND" = windows ]; then
    # Windows: no bash launcher — spawn python.exe -m uvicorn directly (mirrors the
    # runtime's build_windows_sidecar_command). Token via NEXE_TOKEN_INTERNAL env
    # (Windows uvicorn does not read the stdin token). Ollama-only modules.
    # MSYS $$ is NOT a valid Windows PID for OpenProcess — the parent watchdog would fail
    # (err 87) and shut the server down mid-smoke. /proc/$$/winpid maps to the real Win PID
    # of this bash (alive during the health poll), so the watchdog stays happy.
    SMOKE_WINPID=$(cat /proc/$$/winpid 2>/dev/null || echo "$$")
    PYTHONHOME="$SIDECAR_DIR/python-runtime" \
    PYTHONNOUSERSITE=1 PYTHONDONTWRITEBYTECODE=1 PYTHONUNBUFFERED=1 \
    NEXE_TOKEN_INTERNAL="$SMOKE_TOKEN" \
    NEXE_PORT="$SMOKE_PORT" \
    NEXE_SIDECAR=1 \
    NEXE_ENV=production \
    NEXE_PRIMARY_API_KEY="$SMOKE_TOKEN" \
    NEXE_APPROVED_MODULES="security,memory,rag,embeddings,ollama_module" \
    NEXE_HOME="$SIDECAR_DIR/app" \
    NEXE_LOGS_DIR="$SIDECAR_DIR/logs" \
    NEXE_DATA_DIR="$SIDECAR_DIR/data" \
    NEXE_CACHE_DIR="$SIDECAR_DIR/cache" \
    NEXE_QDRANT_PATH="$SIDECAR_DIR/vectors" \
    NEXE_PARENT_PID="$SMOKE_WINPID" \
    NEXE_TRAY_PID="$SMOKE_WINPID" \
    "$BOOT_PY" -m uvicorn core.app:app --host 127.0.0.1 --port "$SMOKE_PORT" \
        --workers 1 --lifespan on --no-access-log --app-dir "$SIDECAR_DIR/app" \
        >"$SMOKE_BOOT_LOG" 2>&1 &
else
    echo "$SMOKE_TOKEN" | \
        NEXE_PORT="$SMOKE_PORT" \
        NEXE_SIDECAR=1 \
        NEXE_ENV=production \
        NEXE_PRIMARY_API_KEY="$SMOKE_TOKEN" \
        NEXE_APPROVED_MODULES="security,memory,rag,embeddings,mlx_module,llama_cpp_module,ollama_module" \
        NEXE_HOME="$SIDECAR_DIR/app" \
        NEXE_LOGS_DIR="$SIDECAR_DIR/logs" \
        NEXE_DATA_DIR="$SIDECAR_DIR/data" \
        NEXE_CACHE_DIR="$SIDECAR_DIR/cache" \
        NEXE_QDRANT_PATH="$SIDECAR_DIR/vectors" \
        NEXE_PARENT_PID="$$" \
        NEXE_TRAY_PID="$$" \
        "$SIDECAR_DIR/nexe-sidecar" >"$SMOKE_BOOT_LOG" 2>&1 &
fi
SIDECAR_PID=$!

# Adapted polling — server-nexe takes 15-25s to become ready (memory + fastembed pre-warm).
# We exit the loop on the first 200 or when we exceed the maximum.
# We disable set -e locally: curl -sf returns a non-zero code while the sidecar is not yet
# accepting connections (ECONNREFUSED), and with set -e active that kills the script.
set +e
HEALTH="FAIL"
SMOKE_ELAPSED=0
while [ "$SMOKE_ELAPSED" -lt "$SMOKE_BOOT_MAX_WAIT" ]; do
    sleep 1
    SMOKE_ELAPSED=$((SMOKE_ELAPSED + 1))
    if ! kill -0 "$SIDECAR_PID" 2>/dev/null; then
        echo "    Sidecar process died at ${SMOKE_ELAPSED}s — boot failed"
        break
    fi
    RESP=$(curl -sf -H "Authorization: Bearer $SMOKE_TOKEN" "http://127.0.0.1:$SMOKE_PORT${HEALTH_PATH}" 2>/dev/null)
    if echo "$RESP" | grep -qE '"status":\s*"(ok|healthy)"'; then
        HEALTH="$RESP"
        echo "    Sidecar ready after ${SMOKE_ELAPSED}s"
        break
    fi
done
set -e

kill "$SIDECAR_PID" 2>/dev/null || true
wait "$SIDECAR_PID" 2>/dev/null || true

# Clean storage created during the smoke test. NEXE_HOME=app/ above
# makes the smoke server fall back to $NEXE_HOME/storage/ for memory + vectors
# + system_core.db + system-logs. Same DEV→bundle contamination pattern as
# .module_cache.json caught earlier. At runtime the Rust spawner sets
# NEXE_STORAGE_PATH to the user-writable location, so this scratch storage
# must never reach the tarball.
rm -rf "$SIDECAR_DIR/app/storage" 2>/dev/null || true

# Also re-strip __pycache__ from venv/ — the smoke test imports
# uvicorn + FastAPI + sidecar modules, and CPython writes fresh .pyc files
# into site-packages/**/__pycache__/ even though Step 6 trimmed them earlier.
# These .pyc are not a notarytool issue (Apple accepted them) but bloat the
# tarball and violate G1 (no transient artifacts in payload).
find "$SIDECAR_DIR/venv" -type d -name "__pycache__" -exec rm -rf {} + 2>/dev/null || true

# Conditional match: POC returns {"status":"ok"}, server-nexe returns {"status":"healthy"...}
if echo "$HEALTH" | grep -qE '"status":\s*"(ok|healthy)"'; then
    echo "    Health check: PASS (authenticated, endpoint $HEALTH_PATH)"
else
    echo "    Health check: FAIL (endpoint $HEALTH_PATH)"
    echo "    Response: $HEALTH"
    echo "    Sidecar boot log: $SMOKE_BOOT_LOG (últimes 20 línies):"
    tail -20 "$SMOKE_BOOT_LOG" 2>/dev/null
    exit 1
fi

# B082 (CRY-01): the server-nexe sidecar must boot with encryption-at-rest
# active. The logic lives in verify-encryption-gate.sh so it is testable
# (test-encryption-gate.sh). POC path (empty APP_SOURCE_DIR) → cleanly skipped.
"$SCRIPT_DIR/verify-encryption-gate.sh" "$APP_SOURCE_DIR" "$SMOKE_BOOT_LOG" || exit 1

# B183/B184: the staged sidecar must not drag in DEV/test data (.test_data,
# worktrees/, runtime storage/, memory *.db). Deterministic, testable gate
# (test-privacy-gate.sh) — last net before packaging; complements the
# rsync excludes (Step 4) and the rm -rf app/storage above.
if [ -d "$SIDECAR_DIR/app" ]; then
    "$SCRIPT_DIR/verify-privacy-gate.sh" "$SIDECAR_DIR/app" || exit 1
fi

# ── Step 8: Report ────────────────────────────────────────────────────
echo ""
echo "════════════════════════════════════════════════════════════════"
echo "  POC SIDECAR BUILD COMPLETE"
echo "════════════════════════════════════════════════════════════════"
echo ""
echo "  Output:     $SIDECAR_DIR"
echo "  Launcher:   $SIDECAR_DIR/nexe-sidecar"
echo "  Python:     $(PYTHONHOME="$SIDECAR_DIR/python-runtime" "$BOOT_PY" --version 2>/dev/null)"
echo "  Arch:       $(uname -m)"
echo "  Bundle size:"
du -sh "$SIDECAR_DIR/venv"
du -sh "$SIDECAR_DIR/app"
du -sh "$SIDECAR_DIR" | awk '{print "  TOTAL: " $1}'
echo ""
echo "  To run manually:"
echo "    $SIDECAR_DIR/nexe-sidecar"
echo ""
echo "  Tauri integration (Fase 2):"
echo "    externalBin or Command::new(\"nexe-sidecar\")"
echo "    with env NEXE_AUTH_TOKEN=<token>"
echo "════════════════════════════════════════════════════════════════"

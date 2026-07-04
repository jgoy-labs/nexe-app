# Building nexe-app — the three installer paths

One product — the Tauri v2 desktop shell + the Python sidecar (server-nexe)
bundled as `sidecar-bundle.tar.gz` and extracted on first launch — shipped to
three targets:

| Target | Artifact | Compiled on | Entry point | Artifact ends up at |
|---|---|---|---|---|
| macOS (arm64) | `.dmg` | the Mac itself | `scripts/build-sidecar.sh` → `pnpm tauri build --bundles dmg` | `src-tauri/target/release/bundle/dmg/` |
| Linux (arm64/x64) | `.AppImage` / `.deb` | a Linux box or VM | same two steps, on the Linux host | `src-tauri/target/release/bundle/{appimage,deb}/` |
| Windows ARM64 | NSIS `-setup.exe` | a Windows 11 ARM64 box/VM, **driven from the Mac** | `scripts/build-windows-installer.sh` (one command) | `win-artifacts/` on the Mac (+ Desktop copy on the target) |

**Principle (all three paths):** the tool and the source of truth live on the
Mac. Build targets are disposable — wipe them and re-run, nothing is lost.
Artifacts must never exist *only* on a build target: the Windows path pulls
the installer back to the Mac with SHA256 verification; treat any path that
leaves the artifact stranded on a VM as a bug.

> CI note: `.github/workflows/release.yml` is a **compilation gate**, not a
> distribution build — it does not produce the Python sidecar (B053). Every
> shippable installer comes from the paths below.

---

## macOS — DMG

Native build on the Mac, from the repo root:

```bash
# 1. Signing env — MUST be exported BEFORE build-sidecar.sh for release builds:
#    if APPLE_SIGNING_IDENTITY is unset the ~333 venv .so binaries are left
#    unsigned and notarization returns Invalid (learned 2026-05-23).
export APPLE_SIGNING_IDENTITY="Developer ID Application: …"

# 2. Sidecar (Python venv + app + fastembed, packaged as tar.gz):
bash scripts/build-sidecar.sh
bash scripts/pre-bundle-sidecar.sh

# 3. Tauri bundle:
pnpm install --frozen-lockfile
pnpm tauri build --bundles dmg
```

Known gotchas: the `hf_xet` package (model downloads hang silently on macOS with it)
is uninstalled by `scripts/build-sidecar.sh` (Step 3b), and `HF_HUB_DISABLE_XET=1` is
set at runtime by the Rust launcher (`src-tauri/src/lib.rs`) — belt and braces, not by
the build scripts. Reproducible-build path remapping + `SOURCE_DATE_EPOCH` for the public
DMG (B141, `scripts/reproducible-build.sh`).

An assisted, checklist-driven version of this path exists as an operator
runbook outside the repo (`/dmg-nexe-app`); this section is the repo-resident
source of truth for the steps themselves.

## Linux — AppImage / deb

Same two-step recipe, run natively on the Linux host (reference setup: a
UTM Linux VM). Points that differ:

- `REQUIREMENTS` stays `requirements.txt`. `requirements-linux.txt` is a
  Linux-only *extra* that `build-sidecar.sh` adds automatically when it
  detects Linux — passing it as the main requirements file produces a broken
  108 MB sidecar (learned 2026-05-23).
- Runtime depends on the system WebKitGTK, which is the fragile piece of this
  target — validate the produced AppImage on the target distro, not only on
  the build box.

```bash
bash scripts/build-sidecar.sh          # on the Linux host
bash scripts/pre-bundle-sidecar.sh
pnpm install --frozen-lockfile
pnpm tauri build --bundles appimage deb
```

## Windows ARM64 — NSIS installer

Fully documented in **[`build-windows-arm64.md`](build-windows-arm64.md)**
(host prerequisites, the remote build script, troubleshooting). Day-to-day you
only need the Mac-driven tool:

```bash
# From the Mac (server-nexe-win + nexe-app-win commits are what gets shipped):
scripts/build-windows-installer.sh [--target user@host]   # default VM
scripts/build-windows-installer.sh --skip-sidecar         # fast tauri-only iteration
```

The tool ships fresh git bundles to the target, builds there (sidecar + NSIS),
stages the installer in `C:/nexe/_outgoing` + the target's Desktop (stale
copies removed), and **retrieves it to `win-artifacts/` on the Mac with SHA256
verification** — the VM owns nothing.

---

## Roadmap

The three paths share the same shape (sidecar → tauri bundle → artifact back
to the Mac) but are three separate tools today. Unifying them behind a single
`build-installer <mac|linux|windows>` entry point is planned (see the
project roadmap); until then, this page is the map.

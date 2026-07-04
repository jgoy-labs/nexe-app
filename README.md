# nexe-app

**App Desktop OSS per server-nexe.**

Shell Tauri v2 que empaqueta server-nexe (sidecar Python amb venv relocatable) i la UI web existent. Afegeix sistema visual de plugins i distribució DMG (macOS) · AppImage (Linux) · installer NSIS (Windows ARM64).

## Principis

- **Thin shell / thick backend:** Tauri = finestra + permisos + lifecycle. Tota la lògica a server-nexe.
- **Sidecar empaquetada:** Release → server-nexe dins l'app. Dev → server-nexe extern.
- **IPC híbrid:** HTTP/WS per dades IA, Tauri Commands per funcions natives.
- **Offline-first:** Funciona en local un cop instal·lat; algunes funcions (baixar models, catàleg de models i token Hugging Face) requereixen xarxa al primer ús.

## Stack

| Capa | Tecnologia |
|---|---|
| Shell | Tauri v2 (Rust stable 1.88+) |
| Frontend | HTML/CSS/JS vanilla existent |
| Node tooling | Node.js 24 LTS + pnpm 10 |
| Backend | Python 3.11+ + FastAPI + Uvicorn |
| Empaquetament | venv relocatable via `build-sidecar.sh` |
| Vector DB | Qdrant |
| LLM | Ollama |
| Packaging | DMG signat/notaritzat (macOS) · AppImage (Linux) · installer NSIS (Windows ARM64, sense signar) |

## Plataformes

- macOS Apple Silicon (principal) — DMG
- Linux x86_64 i ARM64 — AppImage
- Windows 11 ARM64 (des de v1.0.7) — installer NSIS (sense signar; SmartScreen avisa). Backend d'inferència: Ollama.

## Estat

**v1.0.7** — Fases 1-6 completes: shell Tauri + UI empaquetada, onboarding, catàleg de models, token Hugging Face i distribució DMG (macOS) / AppImage (Linux) / installer NSIS Windows ARM64.

## Prerequisits (macOS + Linux)

| Eina | Versió | Instal·lació |
|---|---|---|
| Rust | stable ≥ 1.88 | `rustup update stable` |
| Node.js | 24 LTS o 25.x | `nvm install 24 --lts` |
| pnpm | 10.x | `npm i -g pnpm@10` |
| tauri-cli | ^2.10 | `cargo install tauri-cli --version "^2.10"` |
| macOS | 14+ Sonoma | WKWebView requirement |
| Linux | WebKitGTK 4.1+ | `apt install libwebkit2gtk-4.1-dev` |

## Quickstart

```bash
git clone <repo>
cd nexe-app
pnpm install
cd src-tauri && cargo check
cargo tauri dev   # obre finestra 1024x768 (devUrl http://localhost:1420)
```

## Documentació

- ADRs arquitectònics: `docs/adr/` (19 fitxers amb decisions fonamentals, ADRs 0001-0018 + 0021)
- Contracte API v0 (exemple sidecar): `docs/api-contract-v0.md`
- TEMPLATE.md — guia per clonar aquest starter a una nova app

## Repos relacionats

| Repo | Rol |
|---|---|
| `server-nexe` | Core API (sidecar empaquetada) |
| `plugins-nexe` | Plugins (arquitectura modular) |
| **`nexe-app`** | Shell Tauri + distribució |

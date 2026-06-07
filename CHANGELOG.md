# Changelog

All notable changes to nexe-app are documented here.

Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Versions follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

---

## [Unreleased]

## [1.0.6] — 2026-06-07

### Security

- **CSP: removed `unsafe-eval`** from the isolation frame; hardened sidecar kill/lifecycle handling.
- **Release artifacts are now guarded by a SHA256 integrity check.**
- API key is passed via the URL fragment instead of the query string.

### Added

- **Version-sync gate across 4 sources**, validated against the public repo, plus cross-platform hardening.
- Tauri 2 isolation validator; stale-lock detection; weekly bundle smoke (sidecar placeholder, H-001).

### Fixed

- Body-size check now uses byte length (TextEncoder injected into the test VM).

### Changed

- The public `.gitignore` no longer lists internal tooling entries.

## [1.0.5] — 2026-05-29 · first public beta

> **Public beta.** First public release of nexe-app. Expect rough edges; please
> report issues. Distributed as a single installer (macOS DMG / Linux AppImage)
> that bundles the engine.

### Changed

- **Version aligned with the engine (server-nexe).** The app moves from `0.1.x` to **1.0.5**, matching the engine it bundles. The app and engine ship in a single installer and are perceived as one product, so the app version (source of truth: `package.json` + `tauri.conf.json` + `Cargo.toml`) now follows the distributed product version. This also keeps the Tauri auto-updater consistent (it compares by version number).

### Fixed

- **sidecar: `NEXE_AUTO_INGEST_KNOWLEDGE=1`** — the knowledge base was silently disabled.
- **uninstall: also removes `vectors/` and `storage/`** for a clean re-ingest.
- **catalog sync**: model RAM hints and tier adjustments.

## [0.1.2] — 2026-04-22

### Security — atomic integrity + mutation-verified tests

Hardening pass after an AI-assisted adversarial review. Highlights:

- **TOCTOU verify→serve (P0):** the `plugin://` handler did two separate filesystem reads (verify + serve), letting a local spin-write attacker return bytes that had not just been verified. Fixed with an **atomic snapshot via open file descriptors**: `verify_and_load_plugin_asset` opens all plugin fds before any read, hashes from the in-memory snapshot, and serves bytes from the same snapshot. Invariant: the bytes served are byte-for-byte the bytes that produced the matching hash. A release-only regression test runs a spin-write attacker over 500 requests with zero bypasses. See ADR-0014.
- **Drift-check hardening (P1):** the allowlist drift-check test now strips comments and uses `matchAll` so a decoy comment can't hide a registered command without a validator.
- **Release quality gate (P1):** the `quality-gate` job is now a superset of `check.yml`; no tag publishes without full green checks (fmt, test, clippy, audit, pnpm test/audit, tauri build).
- **9 P2 fixes:** strict `fetch_from_sidecar` URL parsing (Rust + JS), atomic queue bound (CAS counter + RAII guard), hash memory caps (10 MB/file, 50 MB/plugin), full removal of unused Tauri plugins (not just capabilities), CSP host tightening, capability trimming, SHA256SUMS release gate, defensive error-response headers.
- **5 "theater" tests rewritten** to be mutation-verified — each fails when the specific fix is reverted.

### Runtime hotfix

- Unified logging pipeline (`tracing-subscriber` + `tracing-appender`); removed `tauri-plugin-log`, which broke boot via `SetLoggerError`. Release builds persist daily-rotated structured logs to the platform log directory. See ADR-0017.

## [0.1.1] — 2026-04-21

### Security hardening baseline

Cross-review hardening pass — resolved 5 P0, 19 P1, and 22 P2/P3 findings, including:

- TOCTOU mtime integrity-cache bypass → re-hash per request (ADR-0014 v2).
- Isolation allowlist completed (`quit_app`, `get_auth_token`) + CI drift check.
- Release pipeline: draft releases, `SHA256SUMS`, least-privilege permissions, quality gate.
- CSP modern directives (`object-src 'none'`, `base-uri 'self'`, `frame-ancestors 'none'`), capability trimming, `freezePrototype`.
- Crash reports written to the app data dir (mode 0600 on Unix), per-plugin rate limiting, timing-oracle mitigation.
- Reproducible-build baseline (ADR-0015), SBOM (CycloneDX + SPDX), weekly bundle smoke test.

## [0.1.0] — 2026-04-19

### Fase 0 — end-to-end validated on Windows ARM64

First cross-platform GUI validation with manual human tests. 10 runtime bugs were found and fixed that no unit or CI test caught.

### Added

- Tauri v2 shell + system tray (Show/Hide/Quit), `plugin://` URI scheme (ADR-0009).
- Unified Quit flow (X / Alt+F4 / tray / command → one dialog) with re-dispatch guard.
- Auth token baseline (UUID v4) + API contract v0.1 (Bearer).
- `lib.rs` refactored into cohesive modules.
- Release pipeline skeleton (matrix of 4 OSs + SBOM).

### Security

- Plugin integrity SHA-256 (ADR-0014), Isolation Pattern with postMessage firewall (ADR-0013).
- Per-plugin rate limiting (token bucket), strict CSP without `unsafe-inline`.
- `dragDropEnabled: false` (XSS via `File.path`), `STRICT_INTEGRITY` dev/release split.

---

[Unreleased]: https://github.com/jgoy-labs/nexe-app/compare/HEAD...HEAD
[0.1.0]: https://github.com/jgoy-labs/nexe-app/releases/tag/v0.1.0

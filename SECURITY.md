# Security Policy

## Reporting a Vulnerability

This repository is **nexe-app**, the OSS desktop shell (Tauri v2) that packages server-nexe.

- For vulnerabilities in **this app** (e.g., the `plugin://` resolver, CSP
  baseline, CI workflow, resolve-path logic): open a private security advisory
  via the GitHub Security tab of the repo.
- For vulnerabilities in **Tauri, Vite, or transitive dependencies**: report
  directly to the respective upstream projects. See:
  - Tauri: https://github.com/tauri-apps/tauri/security
  - Vite: https://github.com/vitejs/vite/security
  - RustSec advisory DB: https://rustsec.org/

**Please do not report vulnerabilities via public issues.**

## Supported Versions

nexe-app is a released product (current `1.0.7`). Security fixes are applied to
the default branch and shipped in the next signed release; the latest release is
the supported version.

## Security Baseline

Security hardening baseline v2 (2026-04-22, tag `v0.1.2-fase0-v2`).
**Updated after an AI-assisted adversarial review broke v0.1.1** — this baseline closes
the 1 P0 + 2 P1 + 9 P2 + 16 P3 identified findings, rewrites 5 theater tests with
mutation-verified regression, and passes a final adversarial pass with a CLEAR
verdict.

**Principi d'enginyeria crític:** cada fix té un test regression
mutation-verified — el test falla amb codi pre-fix i passa amb el codi post-fix.
Un test que passa amb ambdós és teatre i s'ha eliminat o reescrit.

**CSP + WebView:**
- CSP baseline `default-src 'self'` (C11). The app bundle ships **no inline
  scripts** (its single entry script is external, with SRI), but `script-src`
  and `style-src` deliberately retain `'unsafe-inline'` (B236/B245):
  `dangerousDisableAssetCspModification` is on so Tauri cannot inject nonces, the
  splash + onboarding inject styles at runtime, and the sidecar-served loopback
  web UI carries an inline bootstrap script. Dropping `'unsafe-inline'` is tracked
  separately and needs a render-time gate first — a naive removal broke the
  webview on 2026-06-15 and was reverted in 24 min. Guard: `src/csp.test.js`.
- `'unsafe-eval'` removed from production `script-src` (I-002): it had been added
  for Vite HMR (commit 28750f2), but the `tauri.conf.json` `csp` applies only to
  production builds (dev loads from the Vite dev server). No frontend code, no
  production dependency, and the built `dist/` bundle use `eval()`/`new Function()`.
  Regression guard: `src/csp.test.js` fails if `'unsafe-eval'` reappears.
- Modern directives: `object-src 'none'`, `base-uri 'self'`, `form-action 'self'`, `frame-ancestors 'none'` (C12).
- Per-response hardening headers: `Permissions-Policy`, `Referrer-Policy: no-referrer`,
  `Cross-Origin-Opener-Policy: same-origin`, `X-Frame-Options: SAMEORIGIN` (C51).
- SRI integrity on dist assets (`crossorigin="anonymous" integrity="sha384-..."`) via
  `scripts/add-sri-to-dist.js` build step (C65).
- `freezePrototype: true` — blocks prototype pollution XSS (C22).

**IPC + Isolation:**
- Isolation Pattern **currently disabled** (`"pattern": { "use": "brownfield" }`
  in `tauri.conf.json`, since 2026-05-18 — see ADR-0013). The Tauri isolation
  iframe loads from a custom `isolation-{uuid}://` scheme that CSP source syntax
  cannot allowlist, which blocked app boot; dropping the pattern was the
  workaround (the `isolation` Cargo feature is also off). The
  `isolation-frame/isolation.js` allowlist and its drift-detection CI test (C02)
  are **kept in place** so the pattern can be re-enabled once the CSP issue is
  solved, but **at runtime today no IPC call flows through the isolation hook**.
  Re-enabling is tracked (proper fix: drop `dangerousDisableAssetCspModification`
  and let Tauri auto-inject the isolation origin into the CSP).
- `tauri-plugin-store` + `tauri-plugin-notification` **removed entirely**
  (B9 — not just capability filter): plugin `.init()` calls removed
  from `run()` + `[dependencies]` entries removed from `Cargo.toml`. An
  AI-assisted adversarial review detected that v0.1.1 claimed "removed" but
  had only cleared capabilities; the plugins were still initialised. Binary
  size ~-600KB est., IPC surface -2 plugin command sets. Remaining capabilities:
  `core:default`, `dialog:allow-message`, `dialog:allow-open` (C13 + B9 + B12 —
  `dialog:default` narrowed to the two commands actually used). (`tauri-plugin-deep-link` removed
  2026-05-18 — see `Cargo.toml`; the product no longer relies on OS deep links.)
- `fetch_from_sidecar` URL validation parses structurally via `url::Url::parse`
  (Rust) + `new URL(...)` (JS) — rebutja userinfo hijack, hostname `localhost`,
  IPv6 mapped, wrong scheme, missing port (B2 — an AI-assisted
  adversarial review found the `starts_with("http://127.0.0.1:")` bypass
  accepting 4 PoC vectors).
- `withGlobalTauri: false` (no global `window.__TAURI__`).

**Plugin system:**
- `plugin://` URI scheme with `canonicalize` + per-plugin scope + size cap.
- Integrity SHA-256 with **atomic snapshot verify+load** (B5):
  `verify_and_load_plugin_asset` opens **all plugin file descriptors BEFORE any
  read**, reads all content from the held fds (Unix: inode pinned against
  rename/unlink/write externs; Windows: `File::open` denies exclusive writers),
  hashes from in-memory snapshot, verifies against manifest, and returns the
  requested file's bytes **from the same snapshot**. Bytes served are, by
  invariant, the bytes that produced the matching hash. An AI-assisted review PoC
  against v0.1.1 (separate verify + read) showed **70.5% TOCTOU exploitation**
  rate; v0.1.2 test `b5_verify_and_load_atomic_snapshot_no_bypass` confirms 0%
  under spin-write attacker (release-only, 500 requests).
- Previous algorithm (C01 v0.1.1: re-hash per request via separate `verify` +
  `File::open + read_to_end`) retained only for `HEAD` requests which need no
  body I/O. GET path uses atomic snapshot exclusively.
- Per-file cap `MAX_HASH_FILE_BYTES = 10 MB` + total cap `MAX_HASH_TOTAL_BYTES = 50 MB`
  prevent OOM via sparse/malicious plugin layouts (B6). Test
  `b6_hash_per_file_cap_enforced`.
- Bundle resources glob allowlist prevents accidental `.DS_Store`/`.env`/`.git/`
  in DMG (C17).

**Queue + runtime:**
- Pre-queue bound via **atomic CAS counter** (B3): `PENDING_COUNT:
  AtomicUsize` + `fetch_add(1, AcqRel)` before enqueue; if current `>= MAX_QUEUED`,
  `fetch_sub(1)` + 503. RAII `PendingGuard` decrements on Drop. An AI-assisted
  review showed that v0.1.1 `queued_count() + execute()` non-atomic pattern allowed
  `peak > MAX_QUEUED` under contention; v0.1.2 test `b3_queue_bound_atomic_race`
  confirms strict bound under N=MAX_QUEUED+100 concurrent threads.
- Rate limiting per-plugin 1000 req/s token bucket; burst-resistant.
- `graceful_quit` atomic guard (B1+T1): extracted
  `graceful_quit_try_acquire() -> bool` helper; multiple trigger sources (X /
  Alt+F4 / tray Quit / quit_app command) converge on single dialog. Test
  `t1_dialog_guard_only_one_acquires_under_concurrency` (256 threads + Barrier)
  asserts exactly 1 acquire — mutation-verified against helper pre-fix.
- `tauri::async_runtime::spawn_blocking` for dialog (no runtime starvation) (C40).
- Panic hook writes crash reports to `app_data_dir()/crashes/` mode 0600 Unix
  (not `/tmp/` world-readable); backtrace truncated at 10KB, message sanitized
  against control chars + capped at 1024 chars (C29, C63, B30).
- `err_response` includes defensive headers (Z3): CSP `default-src
  'none'`, X-Content-Type-Options nosniff, Cache-Control no-store,
  Permissions-Policy, Referrer-Policy, X-Frame-Options DENY. Error paths are not
  fingerprinting oracles.

**Auth + secrets:**
- `fetch_from_sidecar` skeleton command injects Bearer token at Rust boundary — main
  webview never sees raw token (C25). `get_auth_token` legacy deprecated with
  `tracing::warn!`.
- Auth token UUID v4 per session (no persistence).

**Observability:**
- Unified logger pipeline: `tracing-subscriber` + `tracing-appender` (daily
  rolling). Single global logger by construction — no `SetLoggerError` class
  of bug. Release builds persist structured logs cross-platform to:
    - macOS: `~/Library/Application Support/com.nexe.app/logs/`
    - Linux: `~/.local/share/com.nexe.app/logs/`
    - Windows: `%LOCALAPPDATA%\com.nexe.app\logs\`
  (ADR-0017). Windows GUI release (`windows_subsystem="windows"`) has no
  stdout; the file layer delivers support visibility there. Bug #2
  real-closed — runtime-verified on macOS. **Correction note:** the previous
  `C15` claim in this section (`tauri-plugin-log active with tracing feature`)
  was empirically false and is retracted in `CHANGELOG.md [0.1.2-hotfix-runtime]`;
  `tauri-plugin-log` is no longer a dependency.
- Control-char sanitization + path truncation 200 chars (log DoS prevention) (C64).

**Supply chain:**
- `cargo audit` blocking CI gate + `.cargo/audit.toml` with `review-date` per CVE
  ignore — `scripts/check-audit-dates.sh` fails CI on expired dates (C18).
- `informational_warnings = []` — forces explicit documentation of each exception
  (no global silencing) (C66).
- `pnpm audit --prod` blocker + `pnpm audit` warning-only (dev deps separated) (C31).
- `pnpm-workspace.yaml` ignoredOptionalDependencies `@rolldown/binding-*` — avoids
  500MB prerelease downloads per CI run (C20).
- Rust toolchain pinned exact (`rust-toolchain.toml channel = "1.94.1"`) for
  reproducibility L1-equivalent baseline (ADR-0015, C59); SLSA L3 deferred.
- Cargo duplicate deps threshold enforced via `scripts/verify.sh` + documented
  in `docs/supply-chain/duplicate-deps.md` (C37).

**Release pipeline:**
- `draft: true` + `SHA256SUMS` generated before upload — no automatic public
  release without maintainer review (C03).
- Release permissions least privilege: `contents: read` workflow, `contents: write`
  only on release job (C09).
- Quality gate (cargo test + clippy + audit + pnpm test) required dependency for
  release job — no tag publishes without green checks (C08).
- `actions/checkout persist-credentials: false` everywhere — GITHUB_TOKEN not
  dropped in `.git/config` workspace (C24).
- Matrix covers macOS arm64 + Linux x64/ARM64 + Windows x64/ARM64.
  (macOS x64 + Universal eliminated 2026-04-23; see release.yml.)
- `concurrency.cancel-in-progress: true` prevents double-tag race (C32).
- SBOM dual format CycloneDX + SPDX (C33).
- Weekly bundle smoke test (full `tauri build` with bundle) (C45).
- **Distribution channel (B053):** the shipped product is a locally-signed,
  notarized DMG (`Developer ID Application`), built out-of-band — **not** by
  this pipeline. The GitHub release workflow is a **verification gate** (quality
  gate + SBOM + SHA256SUMS + `draft: true`), not the distribution channel: its
  `tauri build` step does not bundle the Python sidecar (CI has no Python build
  env, see `weekly-bundle-smoke.yml`) and emits unsigned, not-for-production
  artifacts. Release assets are uploaded from the signed local DMG.

**Starter hygiene:**
- `rename.sh` enriched with author/email/repo/homepage prompts + logo placeholder
  replacement + spike removal option (C39).
- UI default minimal (no "Welcome to Tauri" + branding) — clones don't inherit
  framework branding (C46).
- `authors = ["Jordi Goy..."]` + `publish = false` + repository/homepage metadata
  (C21).
- Window config `minWidth/minHeight/center` prevents UI breakage (C50).

**Code quality:**
- `rustfmt.toml`: `max_width=100`, `imports_granularity=Crate`, `group_imports`.
- `cargo clippy --locked -- -D warnings` blocking gate on lib + bins.
- `cargo clippy --all-targets` warning-only (test code debt tracked).
- MSRV 1.88 CI job (`msrv-check` workflow in `check.yml`, B29) —
  fails if code uses Rust 1.89+ features without bumping declared MSRV.

**Testing discipline:**
- **Mutation testing obligatory for all regression tests:** each test MUST fail
  when the specific line(s) of the fix are reverted (manually verified with a
  `/tmp/` copy of the repo). Tests that pass both pre-fix and post-fix are
  considered "theater" and either rewritten or removed.
- **Test helpers extracted from `#[tauri::command]` bodies** for testability
  without replicating logic in tests (T1 `graceful_quit_try_acquire`, T4
  `emit_deprecation_warning_and_return_token`, T5 `validate_sidecar_method`).
- **Final adversarial gate pre-tag:** an AI-assisted adversarial review attacks the candidate
  tag; if any P0/P1 bypass found, no tag. v0.1.2 passed.

Audit it before using it as a production base. Apply signing/notarization
(macOS) and code-signing (Windows) yourself.

## App Sandbox decision

**`com.apple.security.app-sandbox` is intentionally absent.**

The Python sidecar (server-nexe) requires filesystem access and localhost networking that is incompatible with macOS App Sandbox restrictions. Sandboxing the shell while the sidecar runs outside would create a false sense of security.

**Mitigations in place:**
- `canonicalize` + path traversal checks on all plugin:// requests
- CSP (`default-src 'self'`) + `withGlobalTauri: false` (no global `window.__TAURI__` bridge). Note: the Tauri isolation pattern is currently disabled (brownfield — see "IPC + Isolation" above); re-enabling it is tracked.
- Rate limiting (token bucket, burst-resistant)
- Sidecar auth token (implemented — Bearer injected at the Rust boundary, per-session UUID; see "Auth + secrets" above)
- Process tree lifecycle management (implemented — `src-tauri/src/lifecycle.rs`: Unix group-kill + Windows `taskkill /T`; Windows Job Object `KILL_ON_JOB_CLOSE` implemented in `src-tauri/src/win_job.rs`, sidecar assigned to the job at spawn — K-002, shipped v1.0.7)

This decision is subject to review before the v1 release (hardening).

## Supply chain

This template has a substantial dependency tree:

| Layer | Count | Notes |
|---|---|---|
| Rust crates | ~495 | Audited by `cargo audit` (blocking CI gate) |
| npm packages | ~90 | `pnpm audit` in CI (dev deps included) |
| build.rs scripts | Multiple | Execute at build time — Tauri, sha2, other crates |

**Current mitigations:**
- `cargo audit` with explicit ignores and justifications (`src-tauri/.cargo/audit.toml`)
- Cargo.lock committed (reproducible builds)
- `pnpm-lock.yaml` committed
- `cargo build --locked` enforced in CI
- Reproducible build hygiene with `SOURCE_DATE_EPOCH` + `--remap-path-prefix` (`./scripts/reproducible-build.sh`, ADR-0015)

**Planned (roadmap):**
- SBOM generation (`cargo cyclonedx`)
- Ed25519 plugin signatures
- SLSA provenance attestation

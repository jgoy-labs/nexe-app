//! Onboarding state commands.
//!
//! `check_first_run`          — returns true when the wizard has not been completed.
//! `mark_onboarding_complete` — writes the completion flag to the app config dir.
//!
//! Detection is file-based (not localStorage) so it survives browser storage clears
//! and is consistent across WebView restarts.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use tauri::{AppHandle, Manager};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};

/// Return `true` when the onboarding wizard has not yet been completed.
///
/// Called via `invoke("check_first_run")` at frontend boot.
/// The flag file lives at `<app_config_dir>/onboarding_complete`.
#[tauri::command]
pub fn check_first_run(app: AppHandle) -> bool {
    // Finding B (onboarding loop): first-run is the negation of a completion
    // check that consults BOTH state stores (Rust flag + sidecar onboarding.json)
    // and self-heals their divergence — see `is_onboarding_complete`.
    !is_onboarding_complete(&app)
}

/// Absolute path of the sidecar's OWN onboarding state file. The sidecar
/// (server-nexe core/onboarding_state.py) writes `onboarding.json` under
/// NEXE_DATA_DIR (= `<app_data_dir>/sidecar/data`) the first time
/// `/installer/finalize` succeeds, and its INST-001 guard then 404s any further
/// finalize. This is the SECOND, independent completion store — separate from
/// the Rust flag, which is why the two can diverge and trap the wizard.
fn sidecar_onboarding_json(data_dir: &Path) -> PathBuf {
    data_dir
        .join("sidecar")
        .join("data")
        .join("onboarding.json")
}

/// Pure completion predicate (no `AppHandle`, no side effects) so the two-store
/// convergence logic is unit testable without a Tauri runtime. Onboarding counts
/// as complete when EITHER store says so: the Rust flag
/// `<config_dir>/onboarding_complete` OR the sidecar's `onboarding.json`.
fn is_complete_at(config_dir: &Path, data_dir: &Path) -> bool {
    config_dir.join("onboarding_complete").exists() || sidecar_onboarding_json(data_dir).exists()
}

/// Completion check WITH self-heal (pure w.r.t. its dir args, hence testable).
///
/// Finding B — the onboarding loop: the two stores diverge when a finalize is
/// aborted AFTER the sidecar wrote `onboarding.json` but BEFORE the Rust flag was
/// set. The flag drives first-run, so the wizard keeps re-showing; it re-POSTs
/// `/installer/finalize`, which the sidecar's INST-001 guard 404s because
/// `onboarding.json` already exists → the user is stuck on "No s'ha pogut obtenir
/// la configuració final". Here we CONVERGE the stores: if the flag is missing
/// but the sidecar state is present, write the flag so first-run flips to false
/// on THIS launch and the loop is broken. Best-effort write — a failure just
/// retries next launch (the returned bool is still correct).
fn is_complete_at_healing(config_dir: &Path, data_dir: &Path) -> bool {
    let flag = config_dir.join("onboarding_complete");
    if !flag.exists() && sidecar_onboarding_json(data_dir).exists() {
        let _ = std::fs::create_dir_all(config_dir);
        let _ = std::fs::write(&flag, b"1");
    }
    is_complete_at(config_dir, data_dir)
}

/// `true` when onboarding has completed in EITHER state store, self-healing the
/// flag/onboarding.json divergence that traps the app in the onboarding loop
/// (Finding B). Public so `lib.rs`'s health poll can share the exact same
/// first-run definition as `check_first_run` (they must never disagree).
pub fn is_onboarding_complete(app: &AppHandle) -> bool {
    let config_dir = app.path().app_config_dir().unwrap_or_default();
    let data_dir = app.path().app_data_dir().unwrap_or_default();
    is_complete_at_healing(&config_dir, &data_dir)
}

/// Partial-install state snapshotted at boot, BEFORE `setup_services` extracts
/// the sidecar bundle (which writes `.extracted` for THIS very session).
///
/// TOCTOU guard: a live check at Step 1 would read our own fresh `.extracted`
/// marker as if it came from a previous, aborted session — so every virgin
/// install showed the "Reset installation…" banner. Managed as first thing in
/// `setup_services` (lib.rs), read by `check_partial_install`.
pub struct PartialInstallAtBoot(pub AtomicBool);

/// Pure detection (called ONCE at boot, before extraction): the sidecar bundle
/// was extracted (`.extracted` exists) but the wizard was never completed
/// (`onboarding_complete` absent) — i.e. a genuinely aborted previous session.
pub fn detect_partial_install(app: &AppHandle) -> bool {
    let data_dir = app.path().app_data_dir().unwrap_or_default();
    let extracted = data_dir.join("sidecar").join(".extracted");
    extracted.exists() && !flag_path(app).exists()
}

/// Return the boot-time snapshot of the partial-install state.
///
/// Called via `invoke("check_partial_install")` from Step 1 to decide
/// whether to show the "Reset installation…" banner. Fail-safe: if the
/// snapshot was never taken, report `false` so a virgin user never sees
/// a spurious reset banner.
#[tauri::command]
pub fn check_partial_install(app: AppHandle) -> bool {
    app.try_state::<PartialInstallAtBoot>()
        .map(|s| s.0.load(Ordering::Relaxed))
        .unwrap_or(false)
}

/// Clear all installation state so the next launch re-extracts the
/// sidecar bundle and re-runs the wizard from scratch.
///
/// Removes:
///   - `<app_config_dir>/onboarding_complete`
///   - `<app_data_dir>/sidecar/data/onboarding.json` (sidecar state — the
///     name the sidecar actually writes, see server-nexe core/onboarding_state.py)
///   - `<app_data_dir>/sidecar/.extracted`
///
/// Called via `invoke("reset_installation")` when the user confirms
/// the "Reset installation…" action in Step 1. The frontend reloads
/// the page after this call; step0-splash re-extracts the bundle.
/// Errors are silently ignored — worst case the wizard shows again.
///
/// NEXE-APP-WSA-002: this is a destructive no-arg IPC command (it wipes the
/// onboarding state so the wizard re-runs). Gate it behind a Rust-side native
/// confirmation dialog — exactly as the exit flow does in `lifecycle.rs` — so
/// a raw injected `invoke("reset_installation")` cannot silently clear state
/// without the user acknowledging a modal they cannot script away. Returns
/// `true` only when the user confirmed AND the sweep ran; the frontend reloads
/// regardless (a reload with no reset is a harmless re-render).
#[tauri::command]
pub async fn reset_installation(app: AppHandle) -> bool {
    // Ask through `spawn_blocking` + `blocking_show()` (never on the UI thread
    // and never as a fire-and-forget callback — both deadlock/never-fire on
    // Windows ARM64, see lifecycle.rs). We `await` the answer because, unlike
    // the quit flow, the caller needs the decision synchronously.
    let confirmed = {
        let app = app.clone();
        tauri::async_runtime::spawn_blocking(move || {
            app.dialog()
                .message(
                    "Reset the installation? This clears the onboarding state and \
                     re-runs the setup wizard on the next launch. Your models and \
                     data are kept.",
                )
                .title("Confirm reset")
                .kind(MessageDialogKind::Warning)
                .buttons(MessageDialogButtons::OkCancel)
                .blocking_show()
        })
        .await
        .unwrap_or(false)
    };

    let config_dir = app.path().app_config_dir().unwrap_or_default();
    let data_dir = app.path().app_data_dir().unwrap_or_default();

    // Errors are silently ignored — worst case the wizard shows again, which
    // is a safe degradation for a non-destructive reset.
    match reset_if_confirmed(confirmed, &config_dir, &data_dir) {
        None => false,
        Some(_report) => {
            // The reset just removed `.extracted`, so the boot snapshot is stale
            // by design: clear it so the banner does not reappear after the
            // frontend's `location.reload()`.
            if let Some(snapshot) = app.try_state::<PartialInstallAtBoot>() {
                snapshot.0.store(false, Ordering::Relaxed);
            }
            true
        }
    }
}

/// Confirmation gate for the destructive reset (NEXE-APP-WSA-002), kept pure so
/// the "cancel performs no filesystem mutation" contract is unit testable
/// without a Tauri runtime or a live dialog. Returns `None` (and touches
/// nothing) when the user did not confirm; otherwise runs the paths sweep and
/// returns its report.
fn reset_if_confirmed(
    confirmed: bool,
    config_dir: &std::path::Path,
    data_dir: &std::path::Path,
) -> Option<UninstallReport> {
    if !confirmed {
        return None;
    }
    Some(reset_paths(config_dir, data_dir, false))
}

/// Outcome of an uninstall/reset sweep.
///
/// B058: the previous code discarded every `remove_*` error with `let _ = …`
/// and the tray handler then logged "uninstall complete" unconditionally, so a
/// failed wipe (permissions, file locked, path is a file) left data on disk
/// while telling the user it was gone. We now collect per-path failures so the
/// caller can report honestly. A missing path (`NotFound`) is NOT a failure —
/// it just means there was nothing to remove (idempotent).
#[derive(Debug, Default)]
pub struct UninstallReport {
    /// Human-readable `"<path>: <error>"` for each removal that genuinely failed.
    pub failures: Vec<String>,
}

impl UninstallReport {
    /// `true` when every removal succeeded or had nothing to remove.
    pub fn all_ok(&self) -> bool {
        self.failures.is_empty()
    }
}

fn remove_file_tracked(report: &mut UninstallReport, path: &std::path::Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => report.failures.push(format!("{}: {e}", path.display())),
    }
}

/// `remove_dir_all` that is resilient to Windows' asynchronous handle release.
///
/// BUG 2 (Windows uninstall): after `kill_sidecar_child` (`taskkill /T /F` on the
/// sidecar tree — python.exe + the ollama.exe grandchild), Windows does NOT
/// release the file handles those processes held under the data dir (venv, the
/// extracted sidecar, storage.db, the Qdrant vectors) synchronously. A
/// `remove_dir_all` fired immediately therefore fails with "Access is denied"
/// (os error 5 → `PermissionDenied`) or a sharing violation, and the whole wipe
/// aborts — which is exactly why the uninstall "did nothing" on Windows. We retry
/// with a short backoff so the kernel has time to close the handles.
/// `remove_dir_all` is resumable (already-deleted entries just become NotFound),
/// so retrying continues where it left off. NotFound is success (idempotent).
///
/// POSIX drops handles synchronously on process death, so there is a single
/// attempt and zero behaviour change off Windows.
fn remove_dir_all_resilient(path: &std::path::Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        // Up to 10 retries, backoff 200ms · attempt → ~11s worst case. Generous
        // enough for handle release after a tree kill without hanging the UX
        // (the sweep runs on the blocking pool, and the app exits right after).
        const MAX_ATTEMPTS: u32 = 10;
        let mut attempt: u32 = 0;
        loop {
            match std::fs::remove_dir_all(path) {
                Ok(()) => return Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(e) if attempt >= MAX_ATTEMPTS => return Err(e),
                // Any other error is treated as transient (access denied / sharing
                // violation while handles drain) and retried.
                Err(_) => {
                    attempt += 1;
                    std::thread::sleep(std::time::Duration::from_millis(
                        200u64.saturating_mul(attempt as u64),
                    ));
                }
            }
        }
    }
    #[cfg(not(windows))]
    {
        match std::fs::remove_dir_all(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }
}

fn remove_dir_tracked(report: &mut UninstallReport, path: &std::path::Path) {
    if let Err(e) = remove_dir_all_resilient(path) {
        report.failures.push(format!("{}: {e}", path.display()));
    }
}

/// Full uninstall: removes onboarding state, extracted bundle, downloaded
/// models, AND WebKit localStorage so the wizard starts fresh on next launch.
/// Called from the tray "Uninstall" menu item.
///
/// B058: returns an [`UninstallReport`] so the caller can tell the user the
/// truth instead of claiming success unconditionally. The caller MUST kill the
/// sidecar BEFORE calling this — otherwise the live process keeps writing to
/// `storage`/`vectors` while we delete them (SQLite/Qdrant corruption, or
/// re-created files racing the wipe).
pub fn full_uninstall(app: &AppHandle) -> UninstallReport {
    reset_installation_inner(app, true)
}

fn reset_installation_inner(app: &AppHandle, full: bool) -> UninstallReport {
    let config_dir = app.path().app_config_dir().unwrap_or_default();
    let data_dir = app.path().app_data_dir().unwrap_or_default();
    reset_paths(&config_dir, &data_dir, full)
}

/// Pure removal sweep (no `AppHandle`) so the failure-tracking logic is unit
/// testable without a Tauri runtime.
fn reset_paths(
    config_dir: &std::path::Path,
    data_dir: &std::path::Path,
    full: bool,
) -> UninstallReport {
    let mut report = UninstallReport::default();
    // C2 — ORDER MATTERS: remove the sidecar's state files BEFORE the Rust flag.
    // `is_complete_at_healing` treats "flag absent + onboarding.json present" as a
    // signal to REWRITE the flag; if we deleted the flag first, that exact window
    // would exist mid-reset and a concurrent `is_onboarding_complete()` (or a
    // future re-entrant reset) could resurrect the flag and undo the reset. So:
    // onboarding.json + .finalize_called FIRST, then onboarding_complete.
    //
    // NB: the sidecar persists its state as `onboarding.json` under NEXE_DATA_DIR
    // (= <app_data_dir>/sidecar/data). The old name `onboarding_state.json` never
    // existed on disk — the reset was silently skipping the sidecar state.
    remove_file_tracked(
        &mut report,
        &data_dir
            .join("sidecar")
            .join("data")
            .join("onboarding.json"),
    );
    // Finding B (second, latent loop on the Advanced flow): the sidecar writes
    // `.finalize_called` under NEXE_DATA_DIR and NEVER clears it. If it survives
    // a reset the next finalize is treated as a repeat, re-arming the loop. Remove
    // it on EVERY reset (not only `full`) so a plain reset fully re-arms finalize.
    remove_file_tracked(
        &mut report,
        &data_dir
            .join("sidecar")
            .join("data")
            .join(".finalize_called"),
    );
    // The Rust first-run flag — removed AFTER the sidecar state (see C2 above).
    remove_file_tracked(&mut report, &config_dir.join("onboarding_complete"));
    remove_file_tracked(&mut report, &data_dir.join("sidecar").join(".extracted"));

    if full {
        let sidecar = data_dir.join("sidecar");
        remove_dir_tracked(&mut report, &sidecar.join("data").join("models"));
        remove_dir_tracked(&mut report, &sidecar.join("vectors"));
        remove_dir_tracked(&mut report, &sidecar.join("storage"));

        // WebView storage (localStorage/IndexedDB) lives outside the app data
        // dir, in a platform-specific WebKit location keyed by bundle id.
        #[cfg(target_os = "macos")]
        if let Ok(home) = std::env::var("HOME") {
            let webkit = std::path::PathBuf::from(&home).join("Library/WebKit/com.nexe.app");
            remove_dir_tracked(&mut report, &webkit);
        }

        // WebKitGTK stores its data under XDG data home, which `dirs::data_local_dir()`
        // resolves to on Linux (`~/.local/share`), keyed by the same bundle id used
        // for logs (see logging.rs `APP_DATA_SUBDIR`).
        #[cfg(target_os = "linux")]
        if let Some(local) = dirs::data_local_dir() {
            let webkit = local.join("com.nexe.app");
            remove_dir_tracked(&mut report, &webkit);
        }
    }
    report
}

// ─────────────────────────────────────────────────────────────────────────────
// Finding B — selective uninstall.
//
// The tray's old uninstall was all-or-nothing (`full_uninstall`) and, worse,
// left a lot behind (master.key, .finalize_called, system_core.db, cache/,
// logs/, app/, venv/, the whole com.nexe.app tree, the Keychain token). The user
// wants to CHOOSE what to wipe so a clean reinstall is possible. This block adds:
//   - `UninstallOptions`      — the four checkboxes from the frontend modal;
//   - `is_safe_to_remove`     — the paranoid path guard (defense-in-depth);
//   - `selective_reset_paths` — the pure, testable removal matrix;
//   - `uninstall_with_options`— the gated destructive IPC command.
// ─────────────────────────────────────────────────────────────────────────────

/// Which categories of data an uninstall should remove. Deserialized from the
/// frontend modal's checkbox state via `uninstall_with_options`. `#[serde(default)]`
/// on every field means a missing key defaults to `false` (nothing removed) —
/// fail-safe against a malformed IPC payload.
#[derive(Debug, Default, serde::Deserialize)]
pub struct UninstallOptions {
    /// Downloaded AI models (`<data_dir>/sidecar/data/models`).
    #[serde(default)]
    pub models: bool,
    /// Conversations + persistent memory: Qdrant `vectors` + SQLite `storage`.
    #[serde(default)]
    pub conversations: bool,
    /// Everything: the whole app data + config + platform Library dirs (WebKit,
    /// Logs, Caches, Saved State) — leaves the app "as freshly installed". A
    /// SUPERSET of `models` + `conversations` + onboarding state.
    #[serde(default)]
    pub library: bool,
    /// Ollama's shared model store (`~/.ollama`). Opt-in and INDEPENDENT of
    /// `library` because Ollama is shared with other apps on the machine (we
    /// never touch `/Applications/Ollama.app`).
    #[serde(default)]
    pub ollama: bool,
    /// Embeddings cache (`~/.cache/fastembed`, ~1 GB — the paraphrase-multilingual
    /// model seeded on first launch). ALWAYS removed on a `library` full wipe (the
    /// modal promises "as freshly installed"); opt-in in the per-category mode.
    /// Same path on every OS (`memory/embeddings/paths.py`); accepted by the guard
    /// only as the EXACT path, never a prefix (`~/.cache` is off-limits).
    #[serde(default)]
    pub embeddings_cache: bool,
}

/// Serializable result returned to the frontend by `uninstall_with_options`.
#[derive(Debug, serde::Serialize)]
pub struct UninstallOutcome {
    /// Per-path failures (empty on a fully clean sweep). See [`UninstallReport`].
    pub failures: Vec<String>,
    /// `true` when the user confirmed the native gate and the app is exiting;
    /// `false` when they cancelled it (nothing was touched) or nothing was
    /// selected.
    pub exited: bool,
}

/// Paranoid guard for every recursive `remove_dir_all` (defense-in-depth). A bug
/// in path derivation MUST NEVER let the sweep delete something outside the app's
/// own data. In particular `app_data_dir()`/`app_config_dir()` return an EMPTY
/// path under `unwrap_or_default()` on failure, and `"".join("sidecar")` is the
/// RELATIVE path `sidecar` — deleting that (or `/`, `$HOME`, an ancestor of
/// `$HOME`) would be catastrophic.
///
/// A path is safe to remove ONLY via CONTAINMENT against a known allowlist of
/// bases — NOT by "looks app-owned" heuristics. A presence-based check (scanning
/// for a `com.nexe.app` component or a `.ollama` file name) is exploitable: it
/// would accept an attacker-planted `/tmp/com.nexe.app`, `~/Downloads/com.nexe.app`
/// or `/etc/.ollama`. Containment closes that class entirely. All of:
///   (1) `home` is non-empty — a degenerate `dirs::home_dir()` derives dangerous
///       relative/root paths, so an empty home fails closed GLOBALLY;
///   (2) the path contains NO `..` component — a traversal can `starts_with` a
///       legit base yet resolve outside it (`/base/../../etc`). We reject `..`
///       explicitly rather than `fs::canonicalize`, which errors on the
///       not-yet/never-existing targets an uninstall legitimately sweeps;
///   (3) EITHER the path equals `home/.ollama` EXACTLY (the opt-in shared store,
///       accepted only as itself, never as a prefix), OR it is contained under
///       one of `owned_bases` (data_dir, config_dir, and the derived Library
///       dirs) where that base is non-empty and not `/`. An empty base (""
///       from `unwrap_or_default()`) or `/` can never whitelist anything.
fn is_safe_to_remove(path: &Path, home: &Path, owned_bases: &[&Path]) -> bool {
    // (1) fail closed when home is unknown/degenerate.
    if home.as_os_str().is_empty() {
        return false;
    }
    // (2) reject any traversal outright (before any starts_with can be fooled).
    if path
        .components()
        .any(|c| c == std::path::Component::ParentDir)
    {
        return false;
    }
    // (3a) the Ollama store and the fastembed cache — exact match only, never a
    // prefix. Both live OUTSIDE the app's owned bases (shared/XDG locations), so
    // they are whitelisted individually as themselves; `~/.cache` (the parent)
    // must never match, hence exact equality, not `starts_with`.
    if path == home.join(".ollama") || path == home.join(".cache").join("fastembed") {
        return true;
    }
    // (3b) containment under a KNOWN, non-empty, non-root base.
    owned_bases
        .iter()
        .any(|base| !base.as_os_str().is_empty() && *base != Path::new("/") && path.starts_with(base))
}

/// `remove_dir_all` wrapped in [`is_safe_to_remove`]. On a rejected path it does
/// NOT touch the filesystem and records a failure so the caller reports it
/// instead of silently skipping (B058 honesty) — or, far worse, deleting the
/// wrong tree.
fn remove_dir_guarded(
    report: &mut UninstallReport,
    path: &Path,
    home: &Path,
    owned_bases: &[&Path],
) {
    if !is_safe_to_remove(path, home, owned_bases) {
        report
            .failures
            .push(format!("{}: refused (unsafe path — guard)", path.display()));
        return;
    }
    remove_dir_tracked(report, path);
}

/// `remove_dir_guarded`'s sibling for single FILES (the legacy installer
/// plist): same containment guard, `remove_file` semantics, missing → OK.
fn remove_file_guarded(
    report: &mut UninstallReport,
    path: &Path,
    home: &Path,
    owned_bases: &[&Path],
) {
    if !is_safe_to_remove(path, home, owned_bases) {
        report
            .failures
            .push(format!("{}: refused (unsafe path — guard)", path.display()));
        return;
    }
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => report.failures.push(format!("{}: {e}", path.display())),
    }
}

/// Platform WebView/log/cache/state dirs keyed by bundle id. Returned as owned
/// `PathBuf`s because they are BOTH removal targets (on a library wipe) AND
/// containment bases the guard must accept (they live outside data_dir/config_dir).
/// Derived from `home`.
fn library_data_dirs(home: &Path) -> Vec<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let lib = home.join("Library");
        vec![
            lib.join("WebKit").join("com.nexe.app"),
            lib.join("Logs").join("com.nexe.app"),
            lib.join("Caches").join("com.nexe.app"),
            lib.join("Saved Application State")
                .join("com.nexe.app.savedState"),
        ]
    }
    // Linux: WebKitGTK data + logs live under XDG data home
    // (`~/.local/share/com.nexe.app`, same bundle id as logging.rs).
    #[cfg(target_os = "linux")]
    {
        vec![home.join(".local").join("share").join("com.nexe.app")]
    }
    // Windows (BUG 2): the WebView2 user-data folder (`EBWebView`), the logs
    // (logging.rs writes to `data_local_dir()/com.nexe.app/logs`) and the cache
    // all live under `%LOCALAPPDATA%\com.nexe.app`. Removing that whole dir covers
    // them. `%LOCALAPPDATA%` is `home\AppData\Local` in the default profile
    // layout — derived from `home` to stay consistent with the macOS/Linux
    // entries and the containment guard. (The app data + config live under
    // `%APPDATA%\com.nexe.app`, i.e. `app_data_dir`/`app_config_dir`, already
    // removed via `data_dir`/`config_dir`.)
    #[cfg(target_os = "windows")]
    {
        vec![home
            .join("AppData")
            .join("Local")
            .join("com.nexe.app")]
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = home; // no per-platform Library dirs modelled here
        Vec::new()
    }
}

/// Leftovers from PRE-1.0.7 installs that the current bundle id does not own
/// but this product created (live-verified on a real machine, 2026-07-23:
/// an April/May install left `Application Support/Nexe`, `…/server.nexe` and
/// the legacy installer's Preferences plist behind forever). Removed only on
/// a full `library` wipe — "as freshly installed" must include our own past.
/// The names are ours (legacy branding), never third-party.
fn legacy_leftover_dirs(home: &Path) -> Vec<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let app_support = home.join("Library").join("Application Support");
        vec![app_support.join("Nexe"), app_support.join("server.nexe")]
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = home; // legacy leftovers only modelled on macOS so far
        Vec::new()
    }
}

/// Legacy FILES (not dirs) from the pre-Tauri installer. Same policy as
/// `legacy_leftover_dirs`; removed with `remove_file` (a `remove_dir_all`
/// on a file fails with NotADirectory).
fn legacy_leftover_files(home: &Path) -> Vec<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        vec![home
            .join("Library")
            .join("Preferences")
            .join("net.jgoy.nexe-installer.plist")]
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = home;
        Vec::new()
    }
}

/// Pure selective removal sweep (no `AppHandle`) so the per-option matrix AND the
/// containment guard are unit testable without a Tauri runtime. Every recursive
/// delete goes through [`remove_dir_guarded`] against an EXPLICIT allowlist of
/// bases: `data_dir`, `config_dir`, and the derived Library dirs (so the guard
/// accepts the Library dirs by containment, not by any `com.nexe.app` heuristic).
/// `home` is the derivation root for those dirs, the `.ollama` exact match, and
/// the guard's fail-closed check ($HOME in prod, a tempdir in tests).
pub fn selective_reset_paths(
    config_dir: &Path,
    data_dir: &Path,
    home: &Path,
    opts: &UninstallOptions,
) -> UninstallReport {
    let mut report = UninstallReport::default();

    // The Library dirs are computed once: removal TARGETS on a library wipe and,
    // crucially, CONTAINMENT BASES so the guard accepts them. Owned here so the
    // &Path slice below borrows from a stable Vec.
    let library_dirs = library_data_dirs(home);
    let legacy_dirs = legacy_leftover_dirs(home);
    let legacy_files = legacy_leftover_files(home);
    let mut owned_bases: Vec<&Path> = vec![data_dir, config_dir];
    owned_bases.extend(library_dirs.iter().map(|p| p.as_path()));
    owned_bases.extend(legacy_dirs.iter().map(|p| p.as_path()));
    owned_bases.extend(legacy_files.iter().map(|p| p.as_path()));

    if opts.library {
        // Full wipe: app data + config + the platform Library dirs. SUPERSET of
        // models + conversations + onboarding state, so the per-category branch
        // is skipped.
        remove_dir_guarded(&mut report, data_dir, home, &owned_bases);
        remove_dir_guarded(&mut report, config_dir, home, &owned_bases);
        for dir in &library_dirs {
            remove_dir_guarded(&mut report, dir, home, &owned_bases);
        }
        // Pre-1.0.7 leftovers (2026-07-23, live-verified residue).
        for dir in &legacy_dirs {
            remove_dir_guarded(&mut report, dir, home, &owned_bases);
        }
        for file in &legacy_files {
            remove_file_guarded(&mut report, file, home, &owned_bases);
        }
    } else {
        if opts.models {
            remove_dir_guarded(
                &mut report,
                &data_dir.join("sidecar").join("data").join("models"),
                home,
                &owned_bases,
            );
        }
        if opts.conversations {
            remove_dir_guarded(
                &mut report,
                &data_dir.join("sidecar").join("vectors"),
                home,
                &owned_bases,
            );
            remove_dir_guarded(
                &mut report,
                &data_dir.join("sidecar").join("storage"),
                home,
                &owned_bases,
            );
        }
    }

    // Ollama's shared store — independent of `library` (opt-in, shared). Accepted
    // by the guard only as the EXACT `home/.ollama` path.
    if opts.ollama {
        remove_dir_guarded(&mut report, &home.join(".ollama"), home, &owned_bases);
    }

    // Embeddings cache (`~/.cache/fastembed`, ~1 GB). ALWAYS on a full `library`
    // wipe — otherwise the modal's "as freshly installed" promise is a lie and the
    // cache survives (NEXE-LINUX-FASTEMBED, live-verified on Linux 2026-07-17). Also
    // removable on its own in the per-category mode via the opt-in checkbox. Exact
    // path lives outside `owned_bases`, so the guard accepts it via the (3a) branch.
    if opts.library || opts.embeddings_cache {
        remove_dir_guarded(
            &mut report,
            &home.join(".cache").join("fastembed"),
            home,
            &owned_bases,
        );
    }

    report
}

/// Home dir resolver for the sweep. `dirs::home_dir()` is `None` only in
/// degenerate environments; the empty fallback makes every derived path
/// relative, which the guard's (a) absolute check then rejects — fail-closed.
fn home_dir_or_default() -> PathBuf {
    dirs::home_dir().unwrap_or_default()
}

/// Best-effort removal of the Hugging Face token from the macOS Keychain
/// (generic-password service `nexe-hf-token`, written by the sidecar during
/// onboarding). Only run on a full `library` wipe (the token is part of "config,
/// keys and state"). A missing item (`security` exit 44 = errSecItemNotFound) is
/// NOT a failure — the sweep is idempotent. Non-macOS: no-op.
#[cfg(target_os = "macos")]
fn delete_keychain_token(report: &mut UninstallReport) {
    // Both OUR generic-password items: the HF token AND the server's master
    // encryption key (service `server-nexe`, account `master-encryption-key`
    // — core/crypto/keys.py). Live-verified residue 2026-07-23: a full wipe
    // left the master key orphaned in the Keychain forever. The account is
    // pinned on the second one so only our exact item can ever match.
    let items: [(&str, Option<&str>); 2] = [
        ("nexe-hf-token", None),
        ("server-nexe", Some("master-encryption-key")),
    ];
    for (service, account) in items {
        let mut args = vec!["delete-generic-password", "-s", service];
        if let Some(acct) = account {
            args.extend(["-a", acct]);
        }
        match std::process::Command::new("security").args(&args).output() {
            // Deleted, or nothing to delete (44 = errSecItemNotFound) → idempotent OK.
            Ok(out) if out.status.success() || out.status.code() == Some(44) => {}
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                report
                    .failures
                    .push(format!("keychain {service}: {}", stderr.trim()));
            }
            Err(e) => report.failures.push(format!("keychain {service}: {e}")),
        }
    }
}

/// Linux stores the HF token in the Secret Service (gnome-keyring / KWallet) via
/// the sidecar's `keyring` + `secretstorage` backend (service `nexe-hf-token`,
/// user `default`). The sidecar is already dead at this point (uninstall step 2),
/// so we cannot reuse its Python `keyring`; we shell out to `secret-tool` (from
/// libsecret) instead — each call HARD-timeouted so a locked/unresponsive keyring
/// can never block the app's exit. A surviving token (verified present after the
/// clear) is reported as a failure (B058); a case we cannot even verify (tool
/// missing or timed out) is a best-effort log NOTE, not a failure. NEXE-UNINST-C,
/// live-verified 2026-07-17.
#[cfg(target_os = "linux")]
fn delete_keychain_token(report: &mut UninstallReport) {
    // Run secret-tool on a worker thread with a HARD timeout. A hung/locked Secret
    // Service (waiting on an unlock prompt, or an unresponsive keyring daemon) would
    // otherwise block `.output()` forever → the spawn_blocking task never returns →
    // `app.exit(0)` is never reached → the uninstall hangs with no exit. On timeout
    // we abandon the worker and move on; the imminent app exit reaps any straggler.
    // Returns None on timeout / tool-missing / spawn error.
    fn timed(args: &[&str]) -> Option<std::process::Output> {
        let (tx, rx) = std::sync::mpsc::channel();
        let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        std::thread::spawn(move || {
            let out = std::process::Command::new("secret-tool")
                .args(&owned)
                .stdin(std::process::Stdio::null())
                .output();
            let _ = tx.send(out);
        });
        match rx.recv_timeout(std::time::Duration::from_secs(3)) {
            Ok(Ok(out)) => Some(out),
            _ => None, // timeout, secret-tool missing, or spawn error → best-effort
        }
    }

    // Best-effort clear. Its exit code is deliberately ignored: `secret-tool clear`
    // exits 0 when it removed an item but 1 when NOTHING matched (the tokenless case,
    // common on Ollama-only Linux) — the code lies both ways. The search below is the
    // authority (verified live 2026-07-17).
    let _ = timed(&["clear", "service", "nexe-hf-token", "username", "default"]);

    // Authoritative check: `secret-tool search` exits 0 whether or not it matched,
    // but only PRINTS the entry (stdout) when it EXISTS → empty (or whitespace-only)
    // stdout == absent == cleared. Catches the real failure mode: a keyring where
    // clear silently no-ops and the token survives. When we cannot run/verify at all
    // (timeout, tool missing) an unverifiable removal is NOT proof the token survived
    // → log a best-effort note instead of failing the report.
    match timed(&["search", "service", "nexe-hf-token", "username", "default"]) {
        Some(out) if out.stdout.iter().all(u8::is_ascii_whitespace) => {} // gone → OK
        Some(_) => report
            .failures
            .push("hf token: still present in keyring after clear".to_string()),
        None => tracing::warn!(
            "hf token: could not verify keyring removal (secret-tool missing or unresponsive) — best-effort, token may persist"
        ),
    }

    // The server's master encryption key lives in the same Secret Service
    // (service `server-nexe`, username `master-encryption-key` — the python
    // `keyring` backend maps service/username to these attributes). Same
    // clear-then-verify contract as the token above (2026-07-23).
    let _ = timed(&[
        "clear", "service", "server-nexe", "username", "master-encryption-key",
    ]);
    match timed(&[
        "search", "service", "server-nexe", "username", "master-encryption-key",
    ]) {
        Some(out) if out.stdout.iter().all(u8::is_ascii_whitespace) => {} // gone → OK
        Some(_) => report
            .failures
            .push("master key: still present in keyring after clear".to_string()),
        None => tracing::warn!(
            "master key: could not verify keyring removal (secret-tool missing or unresponsive) — best-effort, key may persist"
        ),
    }
}

// Windows (Credential Manager via cmdkey) is deferred to its own smoke session —
// the token still persists there (NEXE-UNINST-C, Windows arm still open).
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn delete_keychain_token(_report: &mut UninstallReport) {}

/// Human-readable list of what the sweep will remove, shown in the native gate so
/// the user acknowledges the exact scope before anything is deleted.
fn uninstall_summary(opts: &UninstallOptions) -> String {
    if opts.library {
        let mut s = String::from(
            "• All configuration, keys, onboarding state, models and conversations (full wipe)\n• Embeddings cache (~/.cache/fastembed, ~1 GB)",
        );
        if opts.ollama {
            s.push_str("\n• Ollama shared models (~/.ollama)");
        }
        return s;
    }
    let mut lines: Vec<&str> = Vec::new();
    if opts.models {
        lines.push("• Downloaded models");
    }
    if opts.conversations {
        lines.push("• Conversations and memory");
    }
    if opts.embeddings_cache {
        lines.push("• Embeddings cache (~/.cache/fastembed, ~1 GB)");
    }
    if opts.ollama {
        lines.push("• Ollama shared models (~/.ollama)");
    }
    if lines.is_empty() {
        return String::from("• (nothing selected)");
    }
    lines.join("\n")
}

/// Selective uninstall driven by the frontend modal (Finding B). The user picks
/// WHICH categories to remove via [`UninstallOptions`] for a clean reinstall.
///
/// NEXE-APP-WSA-002: this is a DESTRUCTIVE IPC command reachable by any injected
/// `invoke("uninstall_with_options", …)`, so it is gated behind a NATIVE
/// confirmation dialog (spawn_blocking + `blocking_show`, the same pattern as
/// `reset_installation`) that lists exactly what will be removed. A scripted
/// caller cannot dismiss a native modal it did not open, so no wipe happens
/// without a real user acknowledging it. On cancel — or when nothing is selected
/// — it returns `exited: false` and touches nothing.
///
/// On confirm it replicates the tray uninstall's concurrency contract EXACTLY
/// (WSH-001 / B058 / MC-057):
///   1. latch `SHUTDOWN_STARTED` so the supervisor stands down and never respawns
///      the sidecar mid-wipe (which would recreate storage/models);
///   2. kill the sidecar FIRST so it cannot keep writing to the trees we delete;
///   3. run `selective_reset_paths` (paranoid path guard inside);
///   4. (library only) best-effort Keychain delete of the HF token;
///   5. latch `EXIT_CONFIRMED` and `app.exit(0)` — ANY removal kills the running
///      sidecar, so a fresh launch is the only clean state; the modal already
///      warned the user the app will close.
#[tauri::command]
pub async fn uninstall_with_options(app: AppHandle, opts: UninstallOptions) -> UninstallOutcome {
    // Nothing selected → no gate, no exit (protects against an empty/malformed
    // IPC payload and a pointless app restart).
    if !opts.models && !opts.conversations && !opts.library && !opts.ollama && !opts.embeddings_cache
    {
        return UninstallOutcome {
            failures: Vec::new(),
            exited: false,
        };
    }

    // Native gate (WSA-002): never on the UI thread, never fire-and-forget —
    // both deadlock/never-fire on Windows ARM64 (see lifecycle.rs / reset_installation).
    let summary = uninstall_summary(&opts);
    let confirmed = {
        let app = app.clone();
        tauri::async_runtime::spawn_blocking(move || {
            app.dialog()
                .message(format!(
                    "This will remove:\n\n{summary}\n\nThe app will then close. This cannot be undone.\n\nAixò esborrarà el que has triat i tot seguit l'app es tancarà. No es pot desfer."
                ))
                .title("Confirm uninstall")
                .kind(MessageDialogKind::Warning)
                .buttons(MessageDialogButtons::OkCancel)
                .blocking_show()
        })
        .await
        .unwrap_or(false)
    };

    if !confirmed {
        return UninstallOutcome {
            failures: Vec::new(),
            exited: false,
        };
    }

    // ── Tray uninstall concurrency contract (WSH-001 / B058 / MC-057) ──────────
    // 1. Latch SHUTDOWN before the kill so the supervisor won't respawn the
    //    sidecar mid-wipe (recreating storage/models racing the sweep).
    crate::lifecycle::SHUTDOWN_STARTED.store(true, Ordering::Release);
    // 2. Kill the sidecar BEFORE deleting anything it might be writing to
    //    (SQLite/Qdrant corruption, or files re-created racing the wipe).
    if let Some(state) = app.try_state::<crate::SidecarChild>() {
        crate::lifecycle::kill_sidecar_child(&state.0);
    }

    // 3. Selective sweep (guarded) + 4. Keychain — on the BLOCKING pool, never
    //    the async reactor: the Windows-resilient removal sleeps between retries
    //    while the killed sidecar's file handles drain (BUG 2), and blocking the
    //    reactor for seconds would stall other tasks. Move the paths + opts in;
    //    a formatted copy of opts stays behind for logging.
    let config_dir = app.path().app_config_dir().unwrap_or_default();
    let data_dir = app.path().app_data_dir().unwrap_or_default();
    let home = home_dir_or_default();
    let opts_for_log = format!("{opts:?}");
    let report = tauri::async_runtime::spawn_blocking(move || {
        let mut report = selective_reset_paths(&config_dir, &data_dir, &home, &opts);
        // Keychain — only on a full library wipe (token = part of config/keys).
        if opts.library {
            delete_keychain_token(&mut report);
        }
        report
    })
    .await
    .unwrap_or_else(|e| {
        let mut r = UninstallReport::default();
        r.failures.push(format!("uninstall sweep task join error: {e}"));
        r
    });

    if report.all_ok() {
        tracing::info!(opts = %opts_for_log, "selective uninstall complete — exiting");
    } else {
        tracing::error!(opts = %opts_for_log, failures = ?report.failures, "selective uninstall finished with errors");
    }

    // 5. Exit — any removal killed the sidecar; a fresh launch is the clean state.
    // MC-057: EXIT_CONFIRMED before app.exit(0) so ExitRequested does not pop a
    // second "Quit?" dialog.
    crate::lifecycle::EXIT_CONFIRMED.store(true, Ordering::Relaxed);
    let outcome = UninstallOutcome {
        failures: report.failures,
        exited: true,
    };
    app.exit(0);
    outcome
}

/// Return the path of the first-run flag file (pure, testable helper).
pub fn flag_path(app: &AppHandle) -> std::path::PathBuf {
    app.path()
        .app_config_dir()
        .unwrap_or_default()
        .join("onboarding_complete")
}

/// Write the onboarding completion flag to disk.
///
/// Called via `invoke("mark_onboarding_complete")` when the user clicks
/// "Start server-nexe" in Step 5. Creates the config dir if it does not exist.
/// Errors are silently ignored — a failed write means the wizard will show again
/// on next launch, which is a safe degradation.
#[tauri::command]
pub fn mark_onboarding_complete(app: AppHandle) {
    if let Ok(config_dir) = app.path().app_config_dir() {
        let _ = std::fs::create_dir_all(&config_dir);
        let _ = std::fs::write(config_dir.join("onboarding_complete"), b"1");
    }
    // Keep the boot snapshot coherent: once the wizard is complete there is no
    // partial install by definition (matters only if a future flow re-renders
    // Step 1 in the same session, but cheap to keep semantically exact).
    if let Some(snapshot) = app.try_state::<PartialInstallAtBoot>() {
        snapshot.0.store(false, Ordering::Relaxed);
    }
}

// DO NOT add a `get_nexe_api_key` Tauri command: exposing the primary
// api_key via `invoke()` is an XSS exfiltration vector (any compromised
// plugin frame could read it). The wizard receives the api_key in the
// `/installer/finalize` response body instead — see
// src/onboarding/step5-apikey.js.

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use tempfile::TempDir;

    /// Helper: simulate the flag-file logic without a real AppHandle.
    fn flag_exists(dir: &std::path::Path) -> bool {
        dir.join("onboarding_complete").exists()
    }

    fn write_flag(dir: &std::path::Path) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join("onboarding_complete"), b"1").unwrap();
    }

    #[test]
    fn no_flag_means_first_run() {
        let tmp = TempDir::new().unwrap();
        assert!(
            !flag_exists(tmp.path()),
            "fresh dir => first_run should be true"
        );
    }

    #[test]
    fn flag_present_means_not_first_run() {
        let tmp = TempDir::new().unwrap();
        write_flag(tmp.path());
        assert!(
            flag_exists(tmp.path()),
            "flag written => first_run should be false"
        );
    }

    #[test]
    fn write_flag_creates_file() {
        let tmp = TempDir::new().unwrap();
        assert!(!flag_exists(tmp.path()));
        write_flag(tmp.path());
        assert!(flag_exists(tmp.path()));
    }

    // ── check_partial_install helpers ──────────────────────────────────────

    fn partial_install_detected(config_dir: &std::path::Path, data_dir: &std::path::Path) -> bool {
        let extracted = data_dir.join("sidecar").join(".extracted");
        let complete = config_dir.join("onboarding_complete");
        extracted.exists() && !complete.exists()
    }

    fn write_extracted(data_dir: &std::path::Path) {
        let sidecar = data_dir.join("sidecar");
        fs::create_dir_all(&sidecar).unwrap();
        fs::write(sidecar.join(".extracted"), b"sha256-placeholder").unwrap();
    }

    #[test]
    fn fresh_install_no_extracted_not_partial() {
        let cfg = TempDir::new().unwrap();
        let data = TempDir::new().unwrap();
        assert!(!partial_install_detected(cfg.path(), data.path()));
    }

    #[test]
    fn extracted_without_complete_flag_is_partial() {
        let cfg = TempDir::new().unwrap();
        let data = TempDir::new().unwrap();
        write_extracted(data.path());
        assert!(partial_install_detected(cfg.path(), data.path()));
    }

    #[test]
    fn extracted_with_complete_flag_is_not_partial() {
        let cfg = TempDir::new().unwrap();
        let data = TempDir::new().unwrap();
        write_extracted(data.path());
        write_flag(cfg.path());
        assert!(!partial_install_detected(cfg.path(), data.path()));
    }

    // ── reset_installation helpers ─────────────────────────────────────────

    fn do_reset(config_dir: &std::path::Path, data_dir: &std::path::Path) {
        let report = super::reset_paths(config_dir, data_dir, false);
        assert!(report.all_ok(), "reset failures: {:?}", report.failures);
    }

    #[test]
    fn reset_removes_all_three_flags() {
        let cfg = TempDir::new().unwrap();
        let data = TempDir::new().unwrap();

        write_flag(cfg.path());
        write_extracted(data.path());
        let state_path = data.path().join("sidecar").join("data");
        fs::create_dir_all(&state_path).unwrap();
        // The sidecar's real state file name (server-nexe core/onboarding_state.py):
        // guards against regressing to the phantom `onboarding_state.json`.
        fs::write(state_path.join("onboarding.json"), b"{}").unwrap();

        do_reset(cfg.path(), data.path());

        assert!(!cfg.path().join("onboarding_complete").exists());
        assert!(!data.path().join("sidecar").join(".extracted").exists());
        assert!(!state_path.join("onboarding.json").exists());
    }

    #[test]
    fn reset_is_idempotent_on_fresh_dir() {
        let cfg = TempDir::new().unwrap();
        let data = TempDir::new().unwrap();
        // Must not panic when files are absent.
        do_reset(cfg.path(), data.path());
    }

    // ── NEXE-APP-WSA-002: confirmation gate ────────────────────────────────

    #[test]
    fn cancelled_confirmation_performs_no_reset() {
        // A dismissed (or JS-injected, un-acknowledged) confirm must leave the
        // onboarding state fully intact — the whole point of the gate.
        let cfg = TempDir::new().unwrap();
        let data = TempDir::new().unwrap();
        write_flag(cfg.path());
        write_extracted(data.path());

        let out = super::reset_if_confirmed(false, cfg.path(), data.path());

        assert!(out.is_none(), "cancel must skip the reset entirely");
        assert!(
            cfg.path().join("onboarding_complete").exists(),
            "cancel must not delete the completion flag"
        );
        assert!(
            data.path().join("sidecar").join(".extracted").exists(),
            "cancel must not delete the .extracted marker"
        );
    }

    #[test]
    fn confirmed_confirmation_performs_reset() {
        let cfg = TempDir::new().unwrap();
        let data = TempDir::new().unwrap();
        write_flag(cfg.path());
        write_extracted(data.path());

        let out = super::reset_if_confirmed(true, cfg.path(), data.path());

        let report = out.expect("confirm must run the reset");
        assert!(
            report.all_ok(),
            "clean confirmed reset must report ok, got: {:?}",
            report.failures
        );
        assert!(!cfg.path().join("onboarding_complete").exists());
        assert!(!data.path().join("sidecar").join(".extracted").exists());
    }

    // ── B058: full_uninstall failure tracking (reset_paths) ────────────────

    #[test]
    fn uninstall_report_ok_when_everything_removed() {
        let cfg = TempDir::new().unwrap();
        let data = TempDir::new().unwrap();
        write_flag(cfg.path());
        write_extracted(data.path());
        let report = super::reset_paths(cfg.path(), data.path(), true);
        assert!(
            report.all_ok(),
            "clean removal must report ok, got: {:?}",
            report.failures
        );
    }

    #[test]
    fn uninstall_report_ok_when_nothing_to_remove() {
        // Missing paths => NotFound => NOT a failure (idempotent).
        let cfg = TempDir::new().unwrap();
        let data = TempDir::new().unwrap();
        let report = super::reset_paths(cfg.path(), data.path(), true);
        assert!(
            report.all_ok(),
            "absent files must not count as failures, got: {:?}",
            report.failures
        );
    }

    #[test]
    fn uninstall_report_records_real_failure() {
        // `storage` exists as a FILE, so remove_dir_all errors with a non-NotFound
        // kind and must be recorded — proving we no longer silently swallow it.
        let cfg = TempDir::new().unwrap();
        let data = TempDir::new().unwrap();
        let sidecar = data.path().join("sidecar");
        fs::create_dir_all(&sidecar).unwrap();
        fs::write(sidecar.join("storage"), b"not-a-dir").unwrap();

        let report = super::reset_paths(cfg.path(), data.path(), true);
        assert!(
            !report.all_ok(),
            "remove_dir_all on a regular file must be a recorded failure"
        );
        assert!(
            report.failures.iter().any(|f| f.contains("storage")),
            "failure list must name the offending path, got: {:?}",
            report.failures
        );
    }

    // ── Finding B: two-store completion + self-heal (is_complete_at) ───────────

    fn write_sidecar_onboarding_json(data_dir: &std::path::Path) {
        let sd = data_dir.join("sidecar").join("data");
        fs::create_dir_all(&sd).unwrap();
        fs::write(sd.join("onboarding.json"), b"{}").unwrap();
    }

    #[test]
    fn is_complete_true_when_flag_present() {
        let cfg = TempDir::new().unwrap();
        let data = TempDir::new().unwrap();
        write_flag(cfg.path());
        assert!(super::is_complete_at(cfg.path(), data.path()));
    }

    #[test]
    fn is_complete_true_when_sidecar_json_present_without_flag() {
        // The exact loop condition: flag absent, sidecar state present.
        let cfg = TempDir::new().unwrap();
        let data = TempDir::new().unwrap();
        write_sidecar_onboarding_json(data.path());
        assert!(!cfg.path().join("onboarding_complete").exists());
        assert!(super::is_complete_at(cfg.path(), data.path()));
    }

    #[test]
    fn is_complete_false_when_both_absent() {
        let cfg = TempDir::new().unwrap();
        let data = TempDir::new().unwrap();
        assert!(!super::is_complete_at(cfg.path(), data.path()));
    }

    #[test]
    fn self_heal_writes_flag_when_only_sidecar_json_present() {
        // is_complete_at_healing must converge the stores: on flag-absent +
        // onboarding.json-present it writes the flag so first-run flips false.
        let cfg = TempDir::new().unwrap();
        let data = TempDir::new().unwrap();
        write_sidecar_onboarding_json(data.path());
        assert!(!cfg.path().join("onboarding_complete").exists());

        let complete = super::is_complete_at_healing(cfg.path(), data.path());

        assert!(complete, "sidecar state present => onboarding is complete");
        assert!(
            cfg.path().join("onboarding_complete").exists(),
            "self-heal must write the flag so the loop is broken on this launch"
        );
    }

    #[test]
    fn self_heal_noop_when_neither_present() {
        // No sidecar state => nothing to heal, no flag conjured out of thin air.
        let cfg = TempDir::new().unwrap();
        let data = TempDir::new().unwrap();
        assert!(!super::is_complete_at_healing(cfg.path(), data.path()));
        assert!(!cfg.path().join("onboarding_complete").exists());
    }

    // ── Finding B: .finalize_called removed on EVERY reset (not only full) ─────

    #[test]
    fn reset_removes_finalize_called_flag_even_when_not_full() {
        let cfg = TempDir::new().unwrap();
        let data = TempDir::new().unwrap();
        let sd = data.path().join("sidecar").join("data");
        fs::create_dir_all(&sd).unwrap();
        fs::write(sd.join(".finalize_called"), b"1").unwrap();

        // full=false — the latent Advanced-flow loop must be cleared here too.
        let report = super::reset_paths(cfg.path(), data.path(), false);

        assert!(report.all_ok(), "reset failures: {:?}", report.failures);
        assert!(
            !sd.join(".finalize_called").exists(),
            ".finalize_called must be removed on a plain (non-full) reset"
        );
    }

    // ── Finding B: is_safe_to_remove guard (the paranoid path check) ───────────

    #[test]
    fn guard_rejects_attack_paths() {
        // Containment-based guard: presence heuristics are gone, so a path is
        // refused unless it is contained in a KNOWN base (or is exactly
        // home/.ollama). Attack paths that used to slip past the old scan:
        let home = Path::new("/Users/tester");
        let data = Path::new("/Users/tester/Library/Application Support/com.nexe.app");
        let webkit = Path::new("/Users/tester/Library/WebKit/com.nexe.app");
        let bases: [&Path; 2] = [data, webkit];

        // PG1: an attacker-planted dir merely CONTAINING "com.nexe.app" is not
        // under any base → refused (the old component scan accepted it).
        assert!(
            !super::is_safe_to_remove(Path::new("/tmp/com.nexe.app"), home, &bases),
            "/tmp/com.nexe.app must be refused (not contained in a base)"
        );
        assert!(
            !super::is_safe_to_remove(
                Path::new("/Users/tester/Downloads/com.nexe.app"),
                home,
                &bases
            ),
            "~/Downloads/com.nexe.app must be refused (not contained in a base)"
        );
        // A stray ".ollama" anywhere but the EXACT home/.ollama is refused (the
        // old file_name==".ollama" scan accepted any of them).
        assert!(
            !super::is_safe_to_remove(Path::new("/etc/.ollama"), home, &bases),
            "/etc/.ollama must be refused (only the exact home/.ollama is accepted)"
        );
        // PG2: traversal — starts_with(base) WOULD match, but `..` is rejected first.
        assert!(
            !super::is_safe_to_remove(
                Path::new("/Users/tester/Library/WebKit/com.nexe.app/../../../../etc"),
                home,
                &bases
            ),
            "a path containing `..` must be refused"
        );
        // PG5: a base of "/" can never whitelist anything.
        assert!(
            !super::is_safe_to_remove(Path::new("/etc/passwd"), home, &[Path::new("/")]),
            "base=\"/\" must never whitelist"
        );
        // PG3: an empty home fails closed globally.
        assert!(
            !super::is_safe_to_remove(data, Path::new(""), &bases),
            "home=\"\" must fail closed"
        );
        // Empty base ("" from unwrap_or_default) must never whitelist.
        assert!(
            !super::is_safe_to_remove(Path::new("/etc/passwd"), home, &[Path::new("")]),
            "empty base must be ignored"
        );
    }

    #[test]
    fn guard_accepts_real_targets() {
        let home = Path::new("/Users/tester");
        let data = Path::new("/Users/tester/Library/Application Support/com.nexe.app");
        let webkit = Path::new("/Users/tester/Library/WebKit/com.nexe.app");
        let bases: [&Path; 2] = [data, webkit];

        // data_dir itself (library wipe target) — contained (== base).
        assert!(super::is_safe_to_remove(data, home, &bases));
        // A subtree under data_dir (the models dir).
        assert!(super::is_safe_to_remove(
            &data.join("sidecar").join("data").join("models"),
            home,
            &bases
        ));
        // A derived Library dir passed as an explicit base (WebKit).
        assert!(super::is_safe_to_remove(webkit, home, &bases));
        // The Ollama store — accepted as the EXACT home/.ollama.
        assert!(super::is_safe_to_remove(&home.join(".ollama"), home, &bases));
    }

    // ── Finding B: selective_reset_paths matrix ───────────────────────────────

    /// Populate a data_dir + config_dir under a fake HOME like a real install.
    fn make_layout(home: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
        let data = home.join("data");
        let config = home.join("config");
        let sidecar = data.join("sidecar");
        fs::create_dir_all(sidecar.join("data").join("models")).unwrap();
        fs::create_dir_all(sidecar.join("vectors")).unwrap();
        fs::create_dir_all(sidecar.join("storage")).unwrap();
        fs::write(sidecar.join("data").join("onboarding.json"), b"{}").unwrap();
        fs::create_dir_all(&config).unwrap();
        fs::write(config.join("onboarding_complete"), b"1").unwrap();
        (data, config)
    }

    #[test]
    fn selective_only_models_removes_models_keeps_the_rest() {
        let home = TempDir::new().unwrap();
        let (data, config) = make_layout(home.path());
        let opts = super::UninstallOptions {
            models: true,
            ..Default::default()
        };
        let report = super::selective_reset_paths(&config, &data, home.path(), &opts);
        assert!(report.all_ok(), "failures: {:?}", report.failures);
        assert!(!data.join("sidecar").join("data").join("models").exists());
        assert!(data.join("sidecar").join("vectors").exists());
        assert!(data.join("sidecar").join("storage").exists());
        assert!(data.join("sidecar").join("data").join("onboarding.json").exists());
        assert!(config.join("onboarding_complete").exists());
    }

    #[test]
    fn selective_only_conversations_removes_vectors_and_storage() {
        let home = TempDir::new().unwrap();
        let (data, config) = make_layout(home.path());
        let opts = super::UninstallOptions {
            conversations: true,
            ..Default::default()
        };
        let report = super::selective_reset_paths(&config, &data, home.path(), &opts);
        assert!(report.all_ok(), "failures: {:?}", report.failures);
        assert!(data.join("sidecar").join("data").join("models").exists());
        assert!(!data.join("sidecar").join("vectors").exists());
        assert!(!data.join("sidecar").join("storage").exists());
        assert!(data.join("sidecar").join("data").join("onboarding.json").exists());
    }

    #[test]
    fn selective_library_removes_data_and_config_wholesale() {
        let home = TempDir::new().unwrap();
        let (data, config) = make_layout(home.path());
        let opts = super::UninstallOptions {
            library: true,
            ..Default::default()
        };
        let report = super::selective_reset_paths(&config, &data, home.path(), &opts);
        assert!(report.all_ok(), "failures: {:?}", report.failures);
        assert!(!data.exists(), "library wipe removes the whole data dir");
        assert!(!config.exists(), "library wipe removes the whole config dir");
    }

    #[test]
    fn selective_ollama_removes_dot_ollama_only() {
        let home = TempDir::new().unwrap();
        let (data, config) = make_layout(home.path());
        let ollama = home.path().join(".ollama");
        fs::create_dir_all(ollama.join("models")).unwrap();
        let opts = super::UninstallOptions {
            ollama: true,
            ..Default::default()
        };
        let report = super::selective_reset_paths(&config, &data, home.path(), &opts);
        assert!(report.all_ok(), "failures: {:?}", report.failures);
        assert!(!ollama.exists(), "ollama store removed");
        assert!(data.exists(), "ollama-only must not touch app data");
        assert!(config.exists(), "ollama-only must not touch app config");
    }

    #[test]
    fn selective_library_plus_ollama_removes_both() {
        let home = TempDir::new().unwrap();
        let (data, config) = make_layout(home.path());
        let ollama = home.path().join(".ollama");
        fs::create_dir_all(&ollama).unwrap();
        let opts = super::UninstallOptions {
            library: true,
            ollama: true,
            ..Default::default()
        };
        let report = super::selective_reset_paths(&config, &data, home.path(), &opts);
        assert!(report.all_ok(), "failures: {:?}", report.failures);
        assert!(!data.exists() && !config.exists() && !ollama.exists());
    }

    // ── NEXE-LINUX-FASTEMBED: the ~1 GB embeddings cache must not survive ──────

    #[test]
    fn selective_library_removes_fastembed_cache() {
        // The full wipe must clear ~/.cache/fastembed so the modal's "as freshly
        // installed" promise is honest (live-verified residual on Linux 2026-07-17).
        let home = TempDir::new().unwrap();
        let (data, config) = make_layout(home.path());
        let fastembed = home.path().join(".cache").join("fastembed");
        fs::create_dir_all(
            fastembed.join("models--sentence-transformers--paraphrase-multilingual-mpnet-base-v2"),
        )
        .unwrap();
        let opts = super::UninstallOptions {
            library: true,
            ..Default::default()
        };
        let report = super::selective_reset_paths(&config, &data, home.path(), &opts);
        assert!(report.all_ok(), "failures: {:?}", report.failures);
        assert!(
            !fastembed.exists(),
            "library wipe must remove the fastembed cache"
        );
        assert!(!data.exists() && !config.exists());
    }

    #[test]
    fn selective_embeddings_cache_removes_only_fastembed() {
        // Opt-in in the per-category mode: clears the cache, touches nothing else —
        // and crucially NOT the ~/.cache parent nor sibling caches.
        let home = TempDir::new().unwrap();
        let (data, config) = make_layout(home.path());
        let fastembed = home.path().join(".cache").join("fastembed");
        fs::create_dir_all(&fastembed).unwrap();
        let sibling = home.path().join(".cache").join("some-other-app");
        fs::create_dir_all(&sibling).unwrap();
        let opts = super::UninstallOptions {
            embeddings_cache: true,
            ..Default::default()
        };
        let report = super::selective_reset_paths(&config, &data, home.path(), &opts);
        assert!(report.all_ok(), "failures: {:?}", report.failures);
        assert!(!fastembed.exists(), "opt-in must remove the fastembed cache");
        assert!(sibling.exists(), "must not touch other ~/.cache entries");
        assert!(
            data.exists() && config.exists(),
            "opt-in must not touch app data/config"
        );
        assert!(data.join("sidecar").join("data").join("models").exists());
    }

    #[test]
    fn guard_fastembed_exact_only_never_cache_parent() {
        let home = Path::new("/Users/tester");
        let data = Path::new("/Users/tester/Library/Application Support/com.nexe.app");
        let bases: [&Path; 1] = [data];
        // Exact fastembed cache — accepted as itself.
        assert!(super::is_safe_to_remove(
            &home.join(".cache").join("fastembed"),
            home,
            &bases
        ));
        // The parent ~/.cache must NEVER be whitelisted (would nuke every app's cache).
        assert!(
            !super::is_safe_to_remove(&home.join(".cache"), home, &bases),
            "~/.cache (parent) must never be removable"
        );
        // A sibling under ~/.cache is not whitelisted either.
        assert!(
            !super::is_safe_to_remove(&home.join(".cache").join("huggingface"), home, &bases),
            "sibling caches must not be removable"
        );
        // A stray fastembed OUTSIDE home is refused (exact match is home-anchored).
        assert!(
            !super::is_safe_to_remove(Path::new("/tmp/.cache/fastembed"), home, &bases),
            "fastembed outside home must be refused"
        );
    }

    #[test]
    fn selective_library_with_empty_dirs_refuses_and_reports() {
        // Simulate app_data_dir()/app_config_dir() returning "" (unwrap_or_default):
        // the derived paths are empty/relative → the guard must REFUSE them and
        // record a failure, never remove_dir_all a relative path.
        let home = TempDir::new().unwrap();
        let empty = Path::new("");
        let opts = super::UninstallOptions {
            library: true,
            ..Default::default()
        };
        let report = super::selective_reset_paths(empty, empty, home.path(), &opts);
        assert!(
            !report.all_ok(),
            "empty base dirs must produce guard refusals, not silent deletes"
        );
        assert!(
            report.failures.iter().any(|f| f.contains("refused")),
            "failures must name the guard refusal, got: {:?}",
            report.failures
        );
    }

    // ── BUG 2: Windows-resilient removal + platform Library dirs ───────────────

    #[test]
    fn remove_dir_all_resilient_removes_and_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let nuke = tmp.path().join("nuke");
        fs::create_dir_all(nuke.join("me")).unwrap();
        fs::write(nuke.join("me").join("f.txt"), b"x").unwrap();

        // First call removes the tree.
        super::remove_dir_all_resilient(&nuke).expect("should remove");
        assert!(!nuke.exists());
        // Second call on a now-missing dir is Ok (NotFound == success, idempotent).
        super::remove_dir_all_resilient(&nuke).expect("idempotent on missing");
    }

    #[test]
    fn library_data_dirs_are_home_scoped_and_bundle_owned() {
        // On every supported desktop platform the modelled Library dirs must live
        // under home AND carry the bundle id, so the containment guard and the
        // sweep agree (each dir is both a removal target and its own owned base).
        let home = Path::new("/Users/tester");
        let lib_dirs = super::library_data_dirs(home);
        #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
        {
            assert!(
                !lib_dirs.is_empty(),
                "a desktop platform must model at least one Library dir"
            );
            for d in &lib_dirs {
                assert!(d.starts_with(home), "must be under home: {}", d.display());
                assert!(
                    d.components().any(|c| {
                        let s = c.as_os_str().to_string_lossy();
                        s == "com.nexe.app" || s.starts_with("com.nexe.app.")
                    }),
                    "must carry the bundle id: {}",
                    d.display()
                );
                // Passed as its own base, the guard must accept it (containment).
                assert!(
                    super::is_safe_to_remove(d, home, &[d.as_path()]),
                    "guard must accept the modelled Library dir: {}",
                    d.display()
                );
            }
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
        {
            assert!(lib_dirs.is_empty());
        }
    }

    #[test]
    fn legacy_leftovers_are_home_scoped_and_guard_accepted() {
        // FD uninstall fixes (2026-07-23): the pre-1.0.7 leftovers must be
        // under home and accepted by the guard when passed as their own base
        // — same contract as the bundle-owned Library dirs (they carry OUR
        // legacy names, not the bundle id, hence their own test).
        let home = Path::new("/Users/tester");
        #[cfg(target_os = "macos")]
        {
            let dirs = super::legacy_leftover_dirs(home);
            let files = super::legacy_leftover_files(home);
            assert_eq!(dirs.len(), 2, "Nexe + server.nexe");
            assert_eq!(files.len(), 1, "the legacy installer plist");
            for p in dirs.iter().chain(files.iter()) {
                assert!(p.starts_with(home), "home-scoped: {}", p.display());
                assert!(
                    super::is_safe_to_remove(p, home, &[p.as_path()]),
                    "guard must accept the legacy path: {}",
                    p.display()
                );
            }
            assert!(files[0].to_string_lossy().ends_with("net.jgoy.nexe-installer.plist"));
        }
        #[cfg(not(target_os = "macos"))]
        {
            assert!(super::legacy_leftover_dirs(home).is_empty());
            assert!(super::legacy_leftover_files(home).is_empty());
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn full_wipe_sweeps_the_legacy_leftovers() {
        // Live-verified residue 2026-07-23: a full library wipe left
        // `Application Support/Nexe`, `…/server.nexe` and the legacy
        // installer plist behind. Mutation control: removing the legacy
        // sweep from selective_reset_paths turns these to "still exists"
        // (verified RED on the mutant before this commit).
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let app_support = home.join("Library").join("Application Support");
        let legacy_nexe = app_support.join("Nexe");
        let legacy_srv = app_support.join("server.nexe");
        let prefs = home.join("Library").join("Preferences");
        let plist = prefs.join("net.jgoy.nexe-installer.plist");
        fs::create_dir_all(legacy_nexe.join("cfg")).unwrap();
        fs::create_dir_all(&legacy_srv).unwrap();
        fs::create_dir_all(&prefs).unwrap();
        fs::write(&plist, b"plist").unwrap();
        let data_dir = home.join("data");
        let config_dir = home.join("config");
        fs::create_dir_all(&data_dir).unwrap();
        fs::create_dir_all(&config_dir).unwrap();

        let opts = super::UninstallOptions {
            library: true,
            models: false,
            conversations: false,
            ollama: false,
            embeddings_cache: false,
        };
        // NOTE: legacy paths derive from the REAL home in production; here we
        // pass tmp as home so the sweep's derived paths land inside the tmp.
        let report = super::selective_reset_paths(&data_dir, &config_dir, home, &opts);
        assert!(report.all_ok(), "sweep failed: {:?}", report.failures);
        assert!(!legacy_nexe.exists(), "legacy Nexe/ must be swept");
        assert!(!legacy_srv.exists(), "legacy server.nexe/ must be swept");
        assert!(!plist.exists(), "legacy installer plist must be swept");
        // Idempotent: second run on the already-clean tree reports no failures.
        let report2 = super::selective_reset_paths(&data_dir, &config_dir, home, &opts);
        assert!(report2.all_ok(), "not idempotent: {:?}", report2.failures);
    }
}

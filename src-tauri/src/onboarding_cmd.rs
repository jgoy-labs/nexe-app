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

/// Which dir (if any) can only be removed once THIS process is gone (828).
///
/// Pure — no filesystem effects — so the closing message can tell the truth
/// before anything is armed, the same split `plan_app_removal` uses.
///
/// On Windows `%LOCALAPPDATA%\com.nexe.app` holds `EBWebView`, the user-data
/// folder of our OWN live WebView2. The wipe runs before `app.exit(0)`, so that
/// handle is held by a process that is still running by design: the retry loop
/// in `remove_dir_all_resilient` cannot ever win it, and the dir fails with
/// `os error 32` (sharing violation) while the rest of the wipe succeeds.
/// Retrying harder is not the answer — waiting for our own death is.
///
/// Returns `None` off Windows (POSIX unlinks an open path just fine, and the
/// Linux equivalent was measured REFUTED on 17/07: WebKitGTK's cache lives
/// inside the dir the library wipe already removes).
fn plan_deferred_cache_delete(home: &Path, library_wipe_confirmed: bool) -> Option<PathBuf> {
    if !library_wipe_confirmed {
        return None;
    }
    #[cfg(windows)]
    {
        library_data_dirs(home).into_iter().find(|d| d.exists())
    }
    #[cfg(not(windows))]
    {
        let _ = home;
        None
    }
}

/// The detached cmd.exe script: wait (bounded) for `pid` to die, then `rmdir`
/// ONLY if it is really gone. Pure so the contract is unit-tested on every OS;
/// spawn itself is Windows-only.
///
/// 60 × ~1 s (`ping -n 2`) = 60 s, same budget as `self_delete_script`. The
/// delay MUST sit inside the `do` body: putting `& timeout` after the `for`
/// (the first #828 script) ran 120 tight `tasklist` polls, then one timeout,
/// then exited — nexe was still alive, `rmdir` never ran, EBWebView stayed.
/// `ping` not `timeout.exe`: timeout fails when stdin is redirected /
/// CREATE_NO_WINDOW ("Input redirection is not supported"). Quoting: the path
/// goes inside "" and Windows paths cannot contain a literal quote.
#[cfg(any(test, windows))]
fn deferred_dir_delete_script(target: &Path, pid: u32) -> String {
    format!(
        "for /l %i in (1,1,60) do (tasklist /FI \"PID eq {pid}\" | find \"{pid}\" >nul || (rmdir /s /q \"{}\" & exit /b 0) & ping 127.0.0.1 -n 2 >nul)",
        target.display()
    )
}

/// Arm the deferred removal of a dir this process holds open (828).
///
/// Windows counterpart of `spawn_self_delete`: a detached `cmd.exe` polls until
/// `pid` is gone, then removes the tree. Same contract — it MUST be armed after
/// the user's ack (#836), and a failed spawn is reported, never swallowed.
#[cfg(windows)]
fn spawn_deferred_dir_delete(target: &Path, pid: u32) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    // The guard the sweep itself uses: this ends up inside a `rmdir /s /q`.
    let home = home_dir_or_default();
    if !is_safe_to_remove(target, &home, &[target]) {
        return Err(format!(
            "{}: refused (unsafe path — guard)",
            target.display()
        ));
    }
    std::process::Command::new("cmd.exe")
        .args(["/C", &deferred_dir_delete_script(target, pid)])
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("{}: {e}", target.display()))
}

/// The REAL on-disk homes of the user's conversations and persistent memory,
/// derived from `data_dir` (`app_data_dir()`).
///
/// Finding 835 — the previous list pointed at `<data>/sidecar/storage`, which
/// NEVER exists: `remove_dir_all_resilient` maps NotFound to success, so the
/// removal was a silent no-op and the real chat history survived every
/// "Conversations" wipe. Measured on a real install (macOS, 2026-07-31,
/// `~/Library/Application Support/com.nexe.app/sidecar`) and cross-checked
/// against the sidecar code that WRITES each store:
///   - `data/sessions`        — the chat history itself, one encrypted `.enc`
///     file per session. `plugins/web_ui_module/module.py` builds the
///     `SessionManager` with `get_data_dir("sessions")`, and `core/paths/helpers.py`
///     resolves `get_data_dir()` to `$NEXE_DATA_DIR` (= `<data>/sidecar/data`,
///     injected by `spawn_sidecar_process` in lib.rs).
///   - `vectors`              — Qdrant collection + `memory_v1.db` +
///     `metadata_memory.db` (`$NEXE_QDRANT_PATH`, same injection).
///   - `app/storage/memory`   — the memory subsystem's own store; the sidecar's
///     project root is `$NEXE_HOME` = `<data>/sidecar/app` (cwd pinned there),
///     so its `storage/` lives one level deeper than the old guess.
///   - `app/storage/vectors`  — the project-root vector store used by the
///     `lifespan_modules.py` fallback branch (`project_root/storage/vectors`).
///
/// `app/storage/system_core.db` and `app/storage/system-logs` are deliberately
/// NOT here: they are system state, not conversations — the full "everything"
/// wipe removes them with the whole data dir.
fn conversation_dirs(data_dir: &Path) -> Vec<PathBuf> {
    let sidecar = data_dir.join("sidecar");
    let app_storage = sidecar.join("app").join("storage");
    vec![
        sidecar.join("data").join("sessions"),
        sidecar.join("vectors"),
        app_storage.join("memory"),
        app_storage.join("vectors"),
    ]
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
        // Conversations + memory at their REAL paths (finding 835).
        for dir in conversation_dirs(data_dir) {
            remove_dir_tracked(&mut report, &dir);
        }

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
    /// Conversations + persistent memory, at the paths listed in
    /// [`conversation_dirs`] (chat sessions, Qdrant vectors, memory stores).
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
    /// Remove the APPLICATION ITSELF (findings 830/836): the macOS `.app`
    /// bundle or the Linux AppImage, plus our entries in the platform secret
    /// store. Deliberately INDEPENDENT of every data flag above and gated by
    /// its OWN native confirmation — the two questions ("erase my data" and
    /// "uninstall the app") are different questions, and the old single
    /// checkbox answered only the first one while the user believed it
    /// answered both.
    #[serde(default)]
    pub uninstall_app: bool,
}

impl UninstallOptions {
    /// `true` when at least one DATA category is selected. The app-removal
    /// checkbox is not data and is tracked separately (own confirmation).
    fn any_data(&self) -> bool {
        self.models || self.conversations || self.library || self.ollama || self.embeddings_cache
    }
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
    owned_bases.iter().any(|base| {
        !base.as_os_str().is_empty() && *base != Path::new("/") && path.starts_with(base)
    })
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
        vec![home.join("AppData").join("Local").join("com.nexe.app")]
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
/// Finding 838 — until this commit the panic hook wrote its crash reports to
/// `<data_local>/nexe-app/crashes` (bare product name), NOT under the bundle id,
/// so no sweep ever reached them and 0600 stack traces survived every uninstall.
/// `main.rs` now writes under `<data_local>/com.nexe.app/crashes` (already inside
/// the wipe), and this entry sweeps the ORPHANED dir left by every build shipped
/// before the fix. It is OUR own directory (created by our own panic hook), it is
/// added to the LEGACY list on purpose — never to `library_data_dirs`, whose
/// bundle-id invariant (and the ubuntu-22.04 CI assert on it) must hold.
/// `<data_local>` is `~/Library/Application Support` (macOS), `~/.local/share`
/// (Linux) and `%LOCALAPPDATA%` = `home\AppData\Local` (Windows), derived from
/// `home` exactly like `library_data_dirs` so the containment guard agrees.
fn legacy_crash_dir(home: &Path) -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        home.join("Library")
            .join("Application Support")
            .join("nexe-app")
    }
    #[cfg(target_os = "linux")]
    {
        home.join(".local").join("share").join("nexe-app")
    }
    #[cfg(target_os = "windows")]
    {
        home.join("AppData").join("Local").join("nexe-app")
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        home.join("nexe-app")
    }
}

fn legacy_leftover_dirs(home: &Path) -> Vec<PathBuf> {
    // The pre-fix crash-report dir exists on every platform (finding 838).
    // Only the macOS block below pushes, so `mut` is unused elsewhere.
    #[allow(unused_mut)]
    let mut dirs = vec![legacy_crash_dir(home)];
    #[cfg(target_os = "macos")]
    {
        let app_support = home.join("Library").join("Application Support");
        dirs.push(app_support.join("Nexe"));
        dirs.push(app_support.join("server.nexe"));
    }
    dirs
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

// ─────────────────────────────────────────────────────────────────────────────
// Findings 830 / 836 — "Uninstall nexe": removing the APPLICATION itself.
//
// 830: the modal only ever reset DATA. The user ticked "erase everything", the
// app disappeared from the screen and the launcher entry was still there — the
// checkbox lied by omission. 836 (Linux live 17/07): the same wipe left the
// AppImage on the Desktop and then quit with no message, which read as a crash.
//
// A running process cannot delete its own bundle and then keep running, so the
// removal is DELEGATED: we spawn a detached `/bin/sh` that waits for OUR pid to
// disappear and only then removes the artifact. The wait is bounded (60 s) and
// the removal is conditional on the pid being really gone — if the app somehow
// survives, nothing is deleted.
//
// What is NOT implemented, on purpose:
//   - LaunchAgents. MEASURED 2026-07-31: `grep -rn "LaunchAgent\|launchctl"` over
//     this repo returns ZERO hits — nexe-app installs none. The plists present on
//     a developer machine (`com.jgoy.*`) belong to the USER, and deleting an agent
//     we never created would be destroying their configuration.
//   - Windows. We do not self-delete the install tree (that would fight the
//     NSIS package database). After the user's ack we launch `uninstall.exe`
//     next to the exe (#830) and, on a library wipe, a cmd.exe helper that
//     waits for our pid and then removes `%LOCALAPPDATA%\com.nexe.app` (#828).
// ─────────────────────────────────────────────────────────────────────────────

/// Suffix the app artifact must carry on this platform for the guard to accept
/// it as a self-removal target (macOS bundle dir / Linux AppImage file).
#[cfg(target_os = "macos")]
const APP_ARTIFACT_SUFFIX: &str = ".app";
#[cfg(target_os = "linux")]
const APP_ARTIFACT_SUFFIX: &str = ".AppImage";

/// What "Uninstall nexe" can remove from disk on this platform.
#[derive(Debug, PartialEq, Eq)]
pub enum AppArtifact {
    /// A single path that may be deleted once this process is gone.
    SelfRemovable(PathBuf),
    /// We cannot delete ourselves, but the installer left a real uninstaller
    /// next to the executable (Windows/NSIS). Ticking the box hands over to it
    /// instead of doing nothing (830).
    ExternalUninstaller(PathBuf),
    /// We are not the owner of the installed files — the reason is shown to the
    /// user verbatim so the modal never claims a removal it cannot perform.
    NotSelfRemovable(&'static str),
}

/// Resolve the app artifact from the running executable. Pure (both inputs are
/// injected) so every branch is unit-testable without touching the real install.
///
/// - macOS: the nearest ancestor of the executable that ends in `.app`
///   (`/Applications/nexe-app.app/Contents/MacOS/nexe-app` → the bundle).
/// - Linux: `$APPIMAGE`, the absolute path of the running AppImage, exported by
///   the AppImage runtime itself. Absent ⇒ a packaged install (`.deb` under
///   `/usr/bin`), which is the package manager's property, not ours.
/// - Anything else: not self-removable.
fn app_artifact(exe: &Path, appimage: Option<&Path>) -> AppArtifact {
    #[cfg(target_os = "macos")]
    {
        let _ = appimage; // no AppImage on macOS
        match exe
            .ancestors()
            .find(|a| is_safe_app_artifact(a))
            .map(|a| a.to_path_buf())
        {
            Some(bundle) => AppArtifact::SelfRemovable(bundle),
            None => AppArtifact::NotSelfRemovable(
                "not running from an .app bundle — remove the executable by hand / no s'executa des d'un .app: esborra el binari a mà",
            ),
        }
    }
    #[cfg(target_os = "linux")]
    {
        let _ = exe; // the AppImage runtime is the only reliable source
        match appimage {
            Some(p) if is_safe_app_artifact(p) => AppArtifact::SelfRemovable(p.to_path_buf()),
            _ => AppArtifact::NotSelfRemovable(
                "packaged install (.deb) — remove it with your package manager / instal·lació per paquet: fes servir el gestor de paquets",
            ),
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = appimage; // no AppImage off Linux
                          // 830: NSIS drops uninstall.exe next to the executable it installs, so
                          // the running exe's own directory locates it for both a per-user and a
                          // per-machine install — no registry read, no hardcoded %LOCALAPPDATA%.
        match exe.parent().map(|d| d.join(EXTERNAL_UNINSTALLER_NAME)) {
            Some(u) if is_safe_external_uninstaller(&u) && u.is_file() => {
                AppArtifact::ExternalUninstaller(u)
            }
            _ => AppArtifact::NotSelfRemovable(
                "no uninstaller found next to the executable — use Apps & features / no s'ha trobat el desinstal·lador: fes servir Aplicacions i característiques",
            ),
        }
    }
}

/// The file NSIS writes beside the installed executable.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
const EXTERNAL_UNINSTALLER_NAME: &str = "uninstall.exe";

/// Guard for the ONE binary we are allowed to launch on the user's behalf.
/// Same philosophy as [`is_safe_app_artifact`]: the path is derived from
/// `current_exe()`, but it still ends up as an argument to a process spawn, so
/// it is checked rather than trusted. All of:
///   (1) absolute — a relative path would resolve against the spawner's cwd;
///   (2) no `..` component — a traversal could point anywhere;
///   (3) named exactly `uninstall.exe` — never an arbitrary neighbour binary;
///   (4) at least two components below the root, so a root-level dropper
///       (`C:\uninstall.exe`) is refused rather than run.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn is_safe_external_uninstaller(path: &Path) -> bool {
    if !path.is_absolute() {
        return false;
    }
    if path
        .components()
        .any(|c| c == std::path::Component::ParentDir)
    {
        return false;
    }
    let named = path
        .file_name()
        .map(|n| n.eq_ignore_ascii_case(EXTERNAL_UNINSTALLER_NAME))
        .unwrap_or(false);
    let depth = path
        .components()
        .filter(|c| matches!(c, std::path::Component::Normal(_)))
        .count();
    named && depth >= 2
}

/// Paranoid guard for the ONE path the self-delete helper is allowed to touch.
/// Mirrors [`is_safe_to_remove`]'s philosophy (the sweep's guard cannot be
/// reused: the artifact lives outside every app-owned base, in `/Applications`
/// or wherever the user parked the AppImage). All of:
///   (1) absolute — a relative `rm -rf` would resolve against the helper's cwd;
///   (2) no `..` component — a traversal could escape whatever we checked;
///   (3) the file name carries this platform's artifact suffix — a bare
///       directory name can never qualify;
///   (4) at least two path components below the root, so `/x.app` (a root-level
///       artifact) is refused rather than nuked.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn is_safe_app_artifact(path: &Path) -> bool {
    if !path.is_absolute() {
        return false;
    }
    if path
        .components()
        .any(|c| c == std::path::Component::ParentDir)
    {
        return false;
    }
    let named = path
        .file_name()
        .map(|n| n.to_string_lossy().ends_with(APP_ARTIFACT_SUFFIX))
        .unwrap_or(false);
    let depth = path
        .components()
        .filter(|c| matches!(c, std::path::Component::Normal(_)))
        .count();
    named && depth >= 2
}

/// POSIX single-quote a string for `sh -c`. The artifact path is attacker-proof
/// by construction (it comes from `current_exe()`), but it routinely contains
/// spaces (`/Applications/My App.app`) and must survive the shell verbatim.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The detached script: wait (bounded) for `pid` to die, then remove `target`
/// ONLY if it is really gone. Pure so the contract is unit-tested; the sleep
/// budget is 300 × 0.2 s = 60 s, after which we give up WITHOUT deleting.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn self_delete_script(target: &Path, pid: u32) -> String {
    format!(
        "i=0; while kill -0 {pid} 2>/dev/null && [ $i -lt 300 ]; do sleep 0.2; i=$((i+1)); done; \
         kill -0 {pid} 2>/dev/null || rm -rf -- {}",
        sh_quote(&target.to_string_lossy())
    )
}

/// Spawn the detached remover. Returns an error string (for the report) instead
/// of panicking: a failed spawn must be TOLD to the user, not swallowed (B058).
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn spawn_self_delete(target: &Path, pid: u32) -> Result<(), String> {
    if !is_safe_app_artifact(target) {
        return Err(format!(
            "{}: refused (unsafe app artifact — guard)",
            target.display()
        ));
    }
    std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(self_delete_script(target, pid))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("{}: could not spawn the remover: {e}", target.display()))
}

/// Result of the "Uninstall nexe" half, reported in the closing dialog.
#[derive(Debug, PartialEq, Eq)]
pub enum AppRemovalOutcome {
    /// The remover is armed; `PathBuf` disappears once this process exits.
    Scheduled(PathBuf),
    /// The platform's own uninstaller will be launched as we close (830).
    /// Unlike `Scheduled` the removal is not silent: the user finishes it.
    HandOff(PathBuf),
    /// Nothing was armed, and why (shown verbatim).
    Skipped(String),
}

/// Decide what WOULD happen to the app artifact — pure, no filesystem or
/// process effects. Takes the `AppArtifact` already resolved before the
/// confirmation dialog (#836: recomputing it here used to double as the arm
/// trigger, coupling "what to tell the user" with "start the 60s clock").
fn plan_app_removal(artifact: Option<&AppArtifact>) -> AppRemovalOutcome {
    match artifact {
        None => AppRemovalOutcome::Skipped("app removal not requested".to_string()),
        Some(AppArtifact::NotSelfRemovable(reason)) => {
            AppRemovalOutcome::Skipped(reason.to_string())
        }
        Some(AppArtifact::ExternalUninstaller(u)) => AppRemovalOutcome::HandOff(u.clone()),
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        Some(AppArtifact::SelfRemovable(target)) => AppRemovalOutcome::Scheduled(target.clone()),
        // Windows/other never yield SelfRemovable — the arm is unreachable there.
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        Some(AppArtifact::SelfRemovable(target)) => AppRemovalOutcome::Skipped(format!(
            "{}: self-removal is not implemented on this platform",
            target.display()
        )),
    }
}

/// Actually arm the remover for a `Scheduled` plan. #836: this MUST run
/// AFTER the user has acked the closing dialog. The remover's script only
/// waits 60s (300 * 200ms) for `pid` to die before giving up for good — arm
/// it before a modal dialog that waits on a human indefinitely, and a human
/// slower than 60s leaves the artifact alive while the message we already
/// showed promised it gone. `pid` is a parameter (not `std::process::id()`
/// read internally) so this is testable against a throwaway process, the
/// same pattern `spawn_self_delete`'s own tests use.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn arm_app_removal(planned: &AppRemovalOutcome, pid: u32) {
    match planned {
        AppRemovalOutcome::Scheduled(target) => {
            if let Err(e) = spawn_self_delete(target, pid) {
                // The message already promised removal; this is the only trace
                // left if arming itself fails (guard rejection, spawn error).
                tracing::error!(target = %target.display(), error = %e, "app removal failed to arm after user ack");
            }
        }
        // No NSIS off Windows — plan_app_removal never yields HandOff here.
        AppRemovalOutcome::HandOff(u) => {
            tracing::error!(uninstaller = %u.display(), "hand-off planned on a platform with no external uninstaller");
        }
        AppRemovalOutcome::Skipped(_) => {}
    }
}

/// Windows/other: there is no `spawn_self_delete` to call — a `Scheduled` plan
/// cannot be produced here, so it means the two halves have drifted apart.
/// `HandOff` is the real Windows path (830): launch the NSIS uninstaller.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn arm_app_removal(planned: &AppRemovalOutcome, _pid: u32) {
    match planned {
        AppRemovalOutcome::Scheduled(target) => {
            tracing::error!(target = %target.display(), "app removal was scheduled on a platform with no self-delete — nothing armed");
        }
        AppRemovalOutcome::HandOff(uninstaller) => {
            if let Err(e) = spawn_external_uninstaller(uninstaller) {
                tracing::error!(uninstaller = %uninstaller.display(), error = %e, "external uninstaller failed to launch after user ack");
            }
        }
        AppRemovalOutcome::Skipped(_) => {}
    }
}

/// Launch the NSIS uninstaller so it outlives this process (830).
///
/// Outliving us is the whole point: NSIS cannot delete a running binary, which
/// is why it relaunches itself from %TEMP%. No creation flags — a Windows child
/// already survives its parent (the KILL_ON_JOB_CLOSE job is assigned to the
/// sidecar alone, never to this process), and DETACHED_PROCESS + CREATE_NO_WINDOW
/// is the combination the installer smoke lesson warns against. It is also
/// deliberately NOT silent: the user keeps the uninstaller's own confirmation
/// and progress, which is what "Apps & features" would have shown them.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn spawn_external_uninstaller(uninstaller: &Path) -> Result<(), String> {
    if !is_safe_external_uninstaller(uninstaller) {
        return Err(format!(
            "{}: refused (unsafe uninstaller path — guard)",
            uninstaller.display()
        ));
    }
    std::process::Command::new(uninstaller)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("{}: {e}", uninstaller.display()))
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
            // 828: on Windows this is `%LOCALAPPDATA%\com.nexe.app`, held open
            // by our own live WebView2. The in-process sweep cannot win the
            // sharing violation; the deferred cmd.exe helper removes it after
            // we exit. Sweeping it here only fills `report.failures` with a
            // lie the closing dialog then prints next to "removed on close".
            if cfg!(windows) {
                continue;
            }
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
            // Finding 835: the REAL stores (measured), not the phantom
            // `sidecar/storage` the old code silently no-op'd on.
            for dir in conversation_dirs(data_dir) {
                remove_dir_guarded(&mut report, &dir, home, &owned_bases);
            }
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
        "clear",
        "service",
        "server-nexe",
        "username",
        "master-encryption-key",
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

/// Locale-proof presence check over `cmdkey /list:<target>` output.
///
/// Two real-world traps, both VM-verified 2026-07-30:
/// - the NOT-FOUND output echoes the queried name in its localised HEADER
///   ("Credenciales almacenadas en la actualidad para nexe-hf-token:" +
///   "* NINGUNO *") — so "stdout contains the target" flags every clean
///   uninstall as a survivor;
/// - the ENTRY line prints the stored TargetName either PLAIN
///   ("Destino: nexe-hf-token" — what `cmdkey /generic:` and CredWrite with a
///   plain TargetName produce) or with a scheme prefix
///   ("Target: LegacyGeneric:target=nexe-hf-token") — so requiring `target=`
///   misses the plain form.
///
/// The locale-independent rule: take each line's LAST whitespace token —
/// present iff it equals the target (plain entry) or ends with
/// `target=<target>` (prefixed entry). The header never matches (its token
/// carries a trailing `:`), and a compound credential
/// (`master-encryption-key@server-nexe`) can never masquerade as its plain
/// suffix (`server-nexe`). Pure so the contract is unit-tested everywhere.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn cmdkey_stdout_lists_target(stdout: &[u8], target: &str) -> bool {
    let target = target.to_lowercase();
    let prefixed = format!("target={target}");
    String::from_utf8_lossy(stdout)
        .to_lowercase()
        .lines()
        .filter_map(|line| line.split_whitespace().last())
        .any(|token| token == target || token.ends_with(&prefixed))
}

/// Windows stores the HF token in the Credential Manager via the sidecar's
/// `keyring` WinVaultKeyring backend: CRED_TYPE_GENERIC with TargetName =
/// `<service>` and UserName = `<username>` (core/onboarding_state.py:
/// service `nexe-hf-token`, user `default`; master key: core/crypto/keys.py
/// service `server-nexe`, user `master-encryption-key`). When a same-service
/// credential with ANOTHER username pre-existed, WinVaultKeyring re-saves the
/// old one under the compound TargetName `<username>@<service>` — both
/// spellings must go. The sidecar is dead at this point, so we shell out to
/// cmdkey (System32 absolute — PATH is not guaranteed) with the exact
/// clear-then-verify contract of the Linux secret-tool path above:
/// `/delete` exit code is deliberately ignored (it lies both ways),
/// `/list:<target>` decides by STDOUT, each call HARD-timeouted on a worker
/// thread, CREATE_NO_WINDOW so nothing flashes. A verified survivor is a
/// failure (B058); an unverifiable removal is a best-effort warn.
/// NEXE-UNINST-C-WIN (#853), split from NEXE-UNINST-C.
#[cfg(target_os = "windows")]
fn delete_keychain_token(report: &mut UninstallReport) {
    fn timed(args: &[String]) -> Option<std::process::Output> {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000; // same constant as lifecycle.rs:300
        let cmdkey = std::env::var_os("SystemRoot")
            .map(|r| {
                std::path::PathBuf::from(r)
                    .join("System32")
                    .join("cmdkey.exe")
            })
            .unwrap_or_else(|| std::path::PathBuf::from("cmdkey.exe"));
        let owned: Vec<String> = args.to_vec();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let out = std::process::Command::new(cmdkey)
                .args(&owned)
                .stdin(std::process::Stdio::null())
                .creation_flags(CREATE_NO_WINDOW)
                .output();
            let _ = tx.send(out);
        });
        match rx.recv_timeout(std::time::Duration::from_secs(3)) {
            Ok(Ok(out)) => Some(out),
            _ => None, // timeout, cmdkey unrunnable, or spawn error → best-effort
        }
    }

    // (label for the report, TargetName in the Credential Manager)
    let targets: [(&str, &str); 4] = [
        ("hf token", "nexe-hf-token"),
        ("hf token", "default@nexe-hf-token"),
        ("master key", "server-nexe"),
        ("master key", "master-encryption-key@server-nexe"),
    ];
    for (label, target) in targets {
        // Best-effort delete; the /list below is the authority.
        let _ = timed(&[format!("/delete:{target}")]);
        match timed(&[format!("/list:{target}")]) {
            Some(out) if !cmdkey_stdout_lists_target(&out.stdout, target) => {} // gone → OK
            Some(_) => report.failures.push(format!(
                "{label}: credential '{target}' still present in Credential Manager after delete"
            )),
            None => tracing::warn!(
                "{label}: could not verify Credential Manager removal of '{target}' (cmdkey unrunnable or unresponsive) — best-effort, it may persist"
            ),
        }
    }
}

// Other platforms (neither macOS, Linux nor Windows): nothing to clean.
#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn delete_keychain_token(_report: &mut UninstallReport) {}

/// Human-readable list of what the DATA sweep will remove, shown in its own
/// native gate so the user acknowledges the exact scope before anything is
/// deleted. Finding 830: it now says explicitly that the app itself survives —
/// that omission is what made "erase everything" read as "uninstall".
fn data_summary(opts: &UninstallOptions) -> String {
    if opts.library {
        let mut s = String::from(
            "• All configuration, keys, onboarding state, models and conversations (full wipe)\n• Embeddings cache (~/.cache/fastembed, ~1 GB)",
        );
        if opts.ollama {
            s.push_str("\n• Ollama shared models (~/.ollama)");
        }
        s.push_str(
            "\n\nThe app itself stays installed. / L'aplicació segueix instal·lada al disc.",
        );
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
    let mut s = lines.join("\n");
    s.push_str("\n\nThe app itself stays installed. / L'aplicació segueix instal·lada al disc.");
    s
}

/// Scope shown in the SECOND, independent native gate (the "Uninstall nexe"
/// checkbox). It names the artifact that will actually disappear, so the user
/// can recognise it — finding 830 was born of a promise nobody could check.
fn app_removal_summary(artifact: &AppArtifact) -> String {
    match artifact {
        AppArtifact::SelfRemovable(p) => format!(
            "• The application itself / l'aplicació:\n  {}\n• Our entries in the system secret store (Keychain / Credential Manager / keyring)\n\nYour data is only removed if you also ticked the data box. / Les teves dades només s'esborren si també has marcat la casella de dades.",
            p.display()
        ),
        AppArtifact::ExternalUninstaller(p) => format!(
            "• The application itself, via its uninstaller / l'aplicació, amb el seu desinstal·lador:\n  {}\n• Our entries in the system secret store (Keychain / Credential Manager / keyring)\n\nThe uninstaller opens as nexe closes; follow its steps to finish. / El desinstal·lador s'obrirà en tancar-se nexe; segueix-ne els passos per acabar.",
            p.display()
        ),
        AppArtifact::NotSelfRemovable(reason) => format!(
            "• Our entries in the system secret store (Keychain / Credential Manager / keyring)\n\nThe application files CANNOT be removed by nexe itself / els fitxers de l'aplicació NO els pot esborrar el propi nexe:\n  {reason}"
        ),
    }
}

/// Closing message shown BEFORE the app quits (finding 836).
///
/// Live 17/07 on Linux the wipe finished and the process vanished with no word,
/// which the user read as a crash ("sembla que tomba"). The app now states what
/// happened — including failures, and including the case where the app stays
/// installed — and only exits once the user acknowledges it.
fn completion_message(
    report: &UninstallReport,
    data_done: bool,
    app_removal: Option<&AppRemovalOutcome>,
    deferred_cache: Option<&Path>,
) -> String {
    let mut lines: Vec<String> = Vec::new();
    if data_done {
        if report.all_ok() {
            lines.push("• Data erased successfully. / Dades esborrades correctament.".to_string());
        } else {
            lines.push(format!(
                "• Some items could NOT be removed / alguns elements NO s'han pogut esborrar:\n  {}",
                report.failures.join("\n  ")
            ));
        }
    }
    match app_removal {
        Some(AppRemovalOutcome::Scheduled(p)) => lines.push(format!(
            "• The application will be removed as it closes / l'aplicació s'esborrarà en tancar-se:\n  {}",
            p.display()
        )),
        Some(AppRemovalOutcome::HandOff(u)) => lines.push(format!(
            "• The uninstaller will open as nexe closes — finish there to remove the app / el desinstal·lador s'obrirà en tancar-se nexe; acaba-hi per esborrar l'aplicació:\n  {}",
            u.display()
        )),
        Some(AppRemovalOutcome::Skipped(reason)) => lines.push(format!(
            "• The application was NOT removed / l'aplicació NO s'ha esborrat:\n  {reason}"
        )),
        None => lines.push(
            "• The application stays installed and usable. / L'aplicació segueix instal·lada i utilitzable.".to_string(),
        ),
    }
    if let Some(p) = deferred_cache {
        lines.push(format!(
            "• The browser cache is in use until nexe closes; it is removed right after / la memòria cau del navegador s'esborra just en tancar-se nexe:\n  {}",
            p.display()
        ));
    }
    lines.push(String::new());
    lines.push("nexe will close now. / nexe es tancarà ara.".to_string());
    lines.join("\n")
}

/// Native gate on the blocking pool — never on the UI thread, never
/// fire-and-forget (both deadlock/never-fire on Windows ARM64, see
/// lifecycle.rs / reset_installation). Returns the user's answer.
async fn native_confirm(app: &AppHandle, title: &str, body: String) -> bool {
    let app = app.clone();
    let title = title.to_string();
    tauri::async_runtime::spawn_blocking(move || {
        app.dialog()
            .message(body)
            .title(title)
            .kind(MessageDialogKind::Warning)
            .buttons(MessageDialogButtons::OkCancel)
            .blocking_show()
    })
    .await
    .unwrap_or(false)
}

/// Selective uninstall driven by the frontend modal. The user answers TWO
/// independent questions (findings 830/836) and each one has its OWN gate:
///   A. "Erase my data" — the per-category flags of [`UninstallOptions`];
///   B. "Uninstall nexe" — [`UninstallOptions::uninstall_app`], the app itself
///      plus our entries in the platform secret store.
/// Either, neither or both may be ticked; cancelling one gate does NOT cancel
/// the other. The old single gate conflated them, so a user who wanted "erase
/// everything" got a data reset and an app that was still installed (830).
///
/// NEXE-APP-WSA-002: this is a DESTRUCTIVE IPC command reachable by any injected
/// `invoke("uninstall_with_options", …)`, so each half is gated behind a NATIVE
/// confirmation dialog (spawn_blocking + `blocking_show`, the same pattern as
/// `reset_installation`) that lists exactly what will be removed. A scripted
/// caller cannot dismiss a native modal it did not open, so nothing happens
/// without a real user acknowledging it. On cancel — or when nothing is selected
/// — it returns `exited: false` and touches nothing.
///
/// On confirm it replicates the tray uninstall's concurrency contract EXACTLY
/// (WSH-001 / B058 / MC-057):
///   1. latch `SHUTDOWN_STARTED` so the supervisor stands down and never respawns
///      the sidecar mid-wipe (which would recreate storage/models);
///   2. kill the sidecar FIRST so it cannot keep writing to the trees we delete;
///   3. run `selective_reset_paths` (paranoid path guard inside);
///   4. best-effort secret-store delete (full data wipe or app removal — both
///      promise the keys are gone);
///   5. arm the detached app remover if B was confirmed;
///   6. TELL THE USER what happened and wait for the acknowledgement (836 — the
///      silent exit was read as a crash), then latch `EXIT_CONFIRMED` and
///      `app.exit(0)`: any removal killed the sidecar, so a fresh launch is the
///      only clean state.
#[tauri::command]
pub async fn uninstall_with_options(app: AppHandle, opts: UninstallOptions) -> UninstallOutcome {
    // Nothing selected → no gate, no exit (protects against an empty/malformed
    // IPC payload and a pointless app restart).
    if !opts.any_data() && !opts.uninstall_app {
        return UninstallOutcome {
            failures: Vec::new(),
            exited: false,
        };
    }

    // ── Gate A: the data wipe ────────────────────────────────────────────────
    let data_confirmed = if opts.any_data() {
        let summary = data_summary(&opts);
        native_confirm(
            &app,
            "Erase your data",
            format!(
                "This will remove:\n\n{summary}\n\nThis cannot be undone.\n\nAixò esborrarà el que has triat. No es pot desfer."
            ),
        )
        .await
    } else {
        false
    };

    // ── Gate B: the app removal (independent — its own question, own answer) ──
    // The artifact is resolved BEFORE asking so the dialog can name the exact
    // path (or explain why there is nothing we may delete).
    let artifact = if opts.uninstall_app {
        let exe = std::env::current_exe().unwrap_or_default();
        let appimage = std::env::var_os("APPIMAGE").map(PathBuf::from);
        Some(app_artifact(&exe, appimage.as_deref()))
    } else {
        None
    };
    let app_confirmed = match &artifact {
        Some(a) => {
            let summary = app_removal_summary(a);
            native_confirm(
                &app,
                "Uninstall nexe",
                format!(
                    "This will remove:\n\n{summary}\n\nThe app will then close. This cannot be undone.\n\nTot seguit l'app es tancarà. No es pot desfer."
                ),
            )
            .await
        }
        None => false,
    };

    if !data_confirmed && !app_confirmed {
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

    // 3. Selective sweep (guarded) + 4. secret store — on the BLOCKING pool,
    //    never the async reactor: the Windows-resilient removal sleeps between
    //    retries while the killed sidecar's file handles drain (BUG 2), and
    //    blocking the reactor for seconds would stall other tasks. Only the
    //    categories the user CONFIRMED in gate A are swept.
    let config_dir = app.path().app_config_dir().unwrap_or_default();
    let data_dir = app.path().app_data_dir().unwrap_or_default();
    let home = home_dir_or_default();
    let opts_for_log =
        format!("{opts:?} data_confirmed={data_confirmed} app_confirmed={app_confirmed}");
    // The keys are promised gone by BOTH halves: the full data wipe says
    // "configuration, keys and state", and the app removal says the secret
    // store is cleared. Deleting twice is idempotent.
    let wipe_secrets = (data_confirmed && opts.library) || app_confirmed;
    let home_for_deferred = home.clone();
    let library_wipe_confirmed = data_confirmed && opts.library;
    let report = tauri::async_runtime::spawn_blocking(move || {
        let mut report = UninstallReport::default();
        if data_confirmed {
            report = selective_reset_paths(&config_dir, &data_dir, &home, &opts);
        }
        if wipe_secrets {
            delete_keychain_token(&mut report);
        }
        report
    })
    .await
    .unwrap_or_else(|e| {
        let mut r = UninstallReport::default();
        r.failures
            .push(format!("uninstall sweep task join error: {e}"));
        r
    });

    // 5. PLAN app removal (pure — no filesystem/process effects) so the
    //    closing message can name the target truthfully. Reuses `artifact`,
    //    already resolved before gate B's own dialog.
    let app_removal = if app_confirmed {
        Some(plan_app_removal(artifact.as_ref()))
    } else {
        None
    };

    // 5b. PLAN the deferred cache removal (828) — also pure, and also before
    //     the message, so the dialog does not report as a failure something we
    //     are about to handle.
    let deferred_cache = plan_deferred_cache_delete(&home_for_deferred, library_wipe_confirmed);

    if report.all_ok() {
        tracing::info!(opts = %opts_for_log, removal = ?app_removal, "uninstall complete — exiting");
    } else {
        tracing::error!(opts = %opts_for_log, removal = ?app_removal, failures = ?report.failures, "uninstall finished with errors");
    }

    // 6. Say what happened and WAIT for the OK (836: the app used to vanish
    //    mid-uninstall with no word, which reads as a crash).
    let closing = completion_message(
        &report,
        data_confirmed,
        app_removal.as_ref(),
        deferred_cache.as_deref(),
    );
    {
        let app = app.clone();
        let _ = tauri::async_runtime::spawn_blocking(move || {
            app.dialog()
                .message(closing)
                .title("nexe")
                .kind(MessageDialogKind::Info)
                .buttons(MessageDialogButtons::Ok)
                .blocking_show()
        })
        .await;
    }

    // 7. ARM the remover only now, AFTER the human has acked (#836). The
    //    remover's 60s budget starts here, right before we exit — not while
    //    a human was still reading the dialog above.
    if let Some(planned) = &app_removal {
        arm_app_removal(planned, std::process::id());
    }
    // 828: same ordering rule — the cache remover only starts counting once
    // the human is done reading.
    #[cfg(windows)]
    if let Some(target) = &deferred_cache {
        if let Err(e) = spawn_deferred_dir_delete(target, std::process::id()) {
            tracing::error!(target = %target.display(), error = %e, "deferred cache removal failed to arm");
        }
    }

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
        // `vectors` exists as a FILE, so remove_dir_all errors with a non-NotFound
        // kind and must be recorded — proving we no longer silently swallow it.
        // (835: this used to seed `sidecar/storage`, a path the sweep no longer
        // touches because it never existed on a real install — the fixture was
        // validating the fiction instead of the code.)
        let cfg = TempDir::new().unwrap();
        let data = TempDir::new().unwrap();
        let sidecar = data.path().join("sidecar");
        fs::create_dir_all(&sidecar).unwrap();
        fs::write(sidecar.join("vectors"), b"not-a-dir").unwrap();

        let report = super::reset_paths(cfg.path(), data.path(), true);
        assert!(
            !report.all_ok(),
            "remove_dir_all on a regular file must be a recorded failure"
        );
        assert!(
            report.failures.iter().any(|f| f.contains("vectors")),
            "failure list must name the offending path, got: {:?}",
            report.failures
        );
    }

    // ── Finding 835: the conversation stores, at their MEASURED paths ─────────

    #[test]
    fn conversation_dirs_are_the_measured_stores_not_the_phantom() {
        // Measured on a real macOS install 2026-07-31 and cross-checked against
        // the sidecar code that writes each store (see `conversation_dirs`).
        let data = Path::new("/tmp/appdata");
        let dirs = super::conversation_dirs(data);
        let expected = [
            data.join("sidecar").join("data").join("sessions"),
            data.join("sidecar").join("vectors"),
            data.join("sidecar")
                .join("app")
                .join("storage")
                .join("memory"),
            data.join("sidecar")
                .join("app")
                .join("storage")
                .join("vectors"),
        ];
        assert_eq!(dirs, expected, "the sweep must target the real layout");
        // The phantom the old code deleted (a silent no-op) must be gone for good.
        assert!(
            !dirs.contains(&data.join("sidecar").join("storage")),
            "sidecar/storage never existed — it must not be back"
        );
    }

    #[test]
    fn reset_paths_full_removes_the_real_conversation_stores() {
        let cfg = TempDir::new().unwrap();
        let data = TempDir::new().unwrap();
        for dir in super::conversation_dirs(data.path()) {
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("payload"), b"x").unwrap();
        }
        let report = super::reset_paths(cfg.path(), data.path(), true);
        assert!(report.all_ok(), "failures: {:?}", report.failures);
        for dir in super::conversation_dirs(data.path()) {
            assert!(!dir.exists(), "must be swept: {}", dir.display());
        }
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
        assert!(super::is_safe_to_remove(
            &home.join(".ollama"),
            home,
            &bases
        ));
    }

    // ── Finding B: selective_reset_paths matrix ───────────────────────────────

    /// Populate a data_dir + config_dir under a fake HOME like a REAL install.
    ///
    /// 835: the previous fixture created `sidecar/storage`, a directory that
    /// exists on no machine — the test then asserted the sweep removed it and
    /// went green while the user's actual chat history survived. This mirrors
    /// what was measured on disk on 2026-07-31 (macOS install of 18-20/07):
    ///
    /// ```text
    /// sidecar/data/onboarding.json          sidecar/vectors/memory_v1.db
    /// sidecar/data/sessions/<uuid>.enc      sidecar/app/storage/system_core.db
    /// sidecar/data/models/                  sidecar/app/storage/memory/flash/
    ///                                       sidecar/app/storage/vectors/catalog/
    /// ```
    fn make_layout(home: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
        let data = home.join("data");
        let config = home.join("config");
        let sidecar = data.join("sidecar");
        let app_storage = sidecar.join("app").join("storage");
        fs::create_dir_all(sidecar.join("data").join("models")).unwrap();
        fs::create_dir_all(sidecar.join("data").join("sessions")).unwrap();
        fs::write(
            sidecar
                .join("data")
                .join("sessions")
                .join("485f32fd-d3f5-49f8-be6c-1ad7798c01f7.enc"),
            b"encrypted-chat",
        )
        .unwrap();
        fs::create_dir_all(sidecar.join("vectors").join("collection")).unwrap();
        fs::write(sidecar.join("vectors").join("memory_v1.db"), b"sqlite").unwrap();
        fs::create_dir_all(app_storage.join("memory").join("flash")).unwrap();
        fs::create_dir_all(app_storage.join("vectors").join("catalog")).unwrap();
        fs::write(app_storage.join("system_core.db"), b"sqlite").unwrap();
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
        for dir in super::conversation_dirs(&data) {
            assert!(
                dir.exists(),
                "models-only must not touch conversations: {}",
                dir.display()
            );
        }
        assert!(data
            .join("sidecar")
            .join("data")
            .join("onboarding.json")
            .exists());
        assert!(config.join("onboarding_complete").exists());
    }

    #[test]
    fn selective_only_conversations_removes_the_real_stores() {
        // 835 — the chat history (`data/sessions/*.enc`) and the memory stores
        // under `app/storage` must actually disappear. The old code removed
        // `sidecar/vectors` + a non-existent `sidecar/storage`, so the sessions
        // survived a wipe the modal called "conversations and memory".
        let home = TempDir::new().unwrap();
        let (data, config) = make_layout(home.path());
        let session = data
            .join("sidecar")
            .join("data")
            .join("sessions")
            .join("485f32fd-d3f5-49f8-be6c-1ad7798c01f7.enc");
        assert!(session.exists(), "fixture must seed a real session file");
        let opts = super::UninstallOptions {
            conversations: true,
            ..Default::default()
        };
        let report = super::selective_reset_paths(&config, &data, home.path(), &opts);
        assert!(report.all_ok(), "failures: {:?}", report.failures);
        assert!(!session.exists(), "the chat history must be gone");
        for dir in super::conversation_dirs(&data) {
            assert!(!dir.exists(), "must be swept: {}", dir.display());
        }
        // Not conversations: the models, the onboarding state and the system db
        // survive a conversations-only wipe.
        assert!(data.join("sidecar").join("data").join("models").exists());
        assert!(data
            .join("sidecar")
            .join("data")
            .join("onboarding.json")
            .exists());
        assert!(data
            .join("sidecar")
            .join("app")
            .join("storage")
            .join("system_core.db")
            .exists());
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
        assert!(
            !config.exists(),
            "library wipe removes the whole config dir"
        );
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
        assert!(
            !fastembed.exists(),
            "opt-in must remove the fastembed cache"
        );
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
        let dirs = super::legacy_leftover_dirs(home);
        let files = super::legacy_leftover_files(home);
        // The pre-838 crash dir is modelled on EVERY platform.
        assert!(
            dirs.contains(&super::legacy_crash_dir(home)),
            "the orphaned crash dir must be swept: {:?}",
            dirs
        );
        #[cfg(target_os = "macos")]
        {
            assert_eq!(dirs.len(), 3, "crash dir + Nexe + server.nexe");
            assert_eq!(files.len(), 1, "the legacy installer plist");
            assert!(files[0]
                .to_string_lossy()
                .ends_with("net.jgoy.nexe-installer.plist"));
        }
        #[cfg(not(target_os = "macos"))]
        {
            assert_eq!(dirs.len(), 1, "only the crash dir off macOS");
            assert!(files.is_empty());
        }
        for p in dirs.iter().chain(files.iter()) {
            assert!(p.starts_with(home), "home-scoped: {}", p.display());
            assert!(
                super::is_safe_to_remove(p, home, &[p.as_path()]),
                "guard must accept the legacy path: {}",
                p.display()
            );
        }
    }

    #[test]
    fn full_wipe_sweeps_the_orphaned_crash_dir() {
        // 838 — every build before this commit wrote 0600 stack traces to
        // `<data_local>/nexe-app/crashes`, outside the bundle id and therefore
        // outside every sweep. main.rs now writes under `com.nexe.app/`, and the
        // full wipe collects the orphan those older builds left behind.
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let crash_dir = super::legacy_crash_dir(home).join("crashes");
        fs::create_dir_all(&crash_dir).unwrap();
        fs::write(crash_dir.join("crash-1-2.txt"), b"stack trace").unwrap();
        let data_dir = home.join("data");
        let config_dir = home.join("config");
        fs::create_dir_all(&data_dir).unwrap();
        fs::create_dir_all(&config_dir).unwrap();

        let opts = super::UninstallOptions {
            library: true,
            ..Default::default()
        };
        let report = super::selective_reset_paths(&config_dir, &data_dir, home, &opts);
        assert!(report.all_ok(), "sweep failed: {:?}", report.failures);
        assert!(
            !super::legacy_crash_dir(home).exists(),
            "the pre-838 crash dir must be swept"
        );
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
            uninstall_app: false,
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

    // ── #853 (NEXE-UNINST-C-WIN): cmdkey stdout parsing contract ─────────
    // cmdkey is LOCALISED and its exit codes lie (like secret-tool); the only
    // reliable signal is whether /list's stdout echoes the target name. These
    // fixtures are real-shaped outputs (Spanish VM + English) — a parser that
    // always answers "absent" fails the present-cases and vice versa.

    #[test]
    fn cmdkey_parser_detects_presence_across_locales() {
        let es_present = "Credenciales almacenadas en la actualidad:\r\n\r\n\
             Destino: LegacyGeneric:target=nexe-hf-token\r\n    Tipo: Gen\u{e9}rico\r\n\
             Usuario: default\r\n"
            .as_bytes();
        let en_present = b"Currently stored credentials:\r\n\r\n\
             Target: LegacyGeneric:target=nexe-hf-token\r\n    Type: Generic\r\n\
             User: default\r\n";
        let en_upper = b"    Target: LegacyGeneric:target=NEXE-HF-TOKEN\r\n";
        assert!(super::cmdkey_stdout_lists_target(
            es_present,
            "nexe-hf-token"
        ));
        assert!(super::cmdkey_stdout_lists_target(
            en_present,
            "nexe-hf-token"
        ));
        assert!(
            super::cmdkey_stdout_lists_target(en_upper, "nexe-hf-token"),
            "la comparació ha de ser case-insensitive"
        );
        let compound = b"    Target: LegacyGeneric:target=master-encryption-key@server-nexe\r\n";
        assert!(super::cmdkey_stdout_lists_target(
            compound,
            "master-encryption-key@server-nexe"
        ));
        // Capturat REAL a la VM (sessió interactiva, 30/07): l'entrada d'una
        // credencial creada amb `cmdkey /generic:` surt PLANA, sense target= —
        // el tret que va matar el parser v2.
        let es_plain_present = "Credenciales almacenadas en la actualidad para nexe-hf-token:\r\n\r\n    Destino: nexe-hf-token\r\n    Tipo: Gen\u{e9}rico \r\n    Usuario: default\r\n"
            .as_bytes();
        assert!(super::cmdkey_stdout_lists_target(
            es_plain_present,
            "nexe-hf-token"
        ));
    }

    #[test]
    fn cmdkey_parser_compound_never_masquerades_as_plain_suffix() {
        // Si /list:server-nexe llistés també l'entrada composta, la seva línia
        // acaba amb "…@server-nexe" — NO pot comptar com a presència del
        // target pla "server-nexe" (ends-with seria un fals positiu).
        let only_compound = b"    Destino: master-encryption-key@server-nexe\r\n    Usuario: x\r\n";
        assert!(super::cmdkey_stdout_lists_target(
            only_compound,
            "master-encryption-key@server-nexe"
        ));
        assert!(!super::cmdkey_stdout_lists_target(
            only_compound,
            "server-nexe"
        ));
    }

    #[test]
    fn cmdkey_parser_absent_on_error_messages() {
        let es_absent = "CMDKEY: No se puede encontrar el elemento.\r\n".as_bytes();
        let en_absent = b"CMDKEY: Element not found.\r\n";
        // Capturat REAL a la VM (30/07): el not-found FA ECO del target a la
        // capçalera localitzada ("…para nexe-hf-token: * NINGUNO *") — el tret
        // que va matar el parser v1 (hauria marcat cada uninstall net com a
        // supervivent). La presència real només la marca la línia `target=`.
        let es_echo_absent =
            "Credenciales almacenadas en la actualidad para nexe-hf-token:\r\n\r\n* NINGUNO *\r\n"
                .as_bytes();
        let en_echo_absent = b"Currently stored credentials for nexe-hf-token:\r\n\r\n* NONE *\r\n";
        let es_none =
            "Credenciales almacenadas en la actualidad:\r\n\r\n* NINGUNO *\r\n".as_bytes();
        let empty = b"";
        for out in [
            es_absent,
            en_absent,
            es_echo_absent,
            en_echo_absent,
            es_none,
            empty,
        ] {
            assert!(
                !super::cmdkey_stdout_lists_target(out, "nexe-hf-token"),
                "cap variant not-found pot semblar presència: {:?}",
                String::from_utf8_lossy(out)
            );
        }
    }

    /// Gate empíric del #853 a la VM Windows ARM64 — executa explícitament:
    /// `cargo test -- --ignored cmdkey_roundtrip`. Sembra una credencial REAL,
    /// verifica presència (mutation-control del parser en viu), passa pel codi
    /// de PRODUCCIÓ i verifica absència + report net.
    #[cfg(target_os = "windows")]
    #[test]
    #[ignore = "toca el Credential Manager real — gate de VM per a #853"]
    fn cmdkey_roundtrip_real_credential() {
        use std::process::Command;

        let seed = Command::new("cmdkey")
            .args([
                "/generic:nexe-hf-token",
                "/user:default",
                "/pass:dummy-secret-853",
            ])
            .output()
            .expect("cmdkey /generic ha de poder executar-se");
        assert!(seed.status.success(), "seed: {:?}", seed);

        let listed = Command::new("cmdkey")
            .args(["/list:nexe-hf-token"])
            .output()
            .expect("cmdkey /list");
        assert!(
            super::cmdkey_stdout_lists_target(&listed.stdout, "nexe-hf-token"),
            "mutation-control: la credencial sembrada s'ha de VEURE abans del delete: {}",
            String::from_utf8_lossy(&listed.stdout)
        );

        let mut report = super::UninstallReport::default();
        super::delete_keychain_token(&mut report);
        assert!(report.all_ok(), "failures: {:?}", report.failures);

        let after = Command::new("cmdkey")
            .args(["/list:nexe-hf-token"])
            .output()
            .expect("cmdkey /list post-delete");
        assert!(
            !super::cmdkey_stdout_lists_target(&after.stdout, "nexe-hf-token"),
            "la credencial ha sobreviscut al delete: {}",
            String::from_utf8_lossy(&after.stdout)
        );
    }

    // ── Findings 830 / 836: two independent questions, one closing message ────

    #[test]
    fn any_data_ignores_the_app_checkbox() {
        // The two halves must never leak into each other: ticking ONLY "uninstall
        // the app" must not make the command believe a data wipe was requested.
        let app_only = super::UninstallOptions {
            uninstall_app: true,
            ..Default::default()
        };
        assert!(!app_only.any_data());
        let data_only = super::UninstallOptions {
            conversations: true,
            ..Default::default()
        };
        assert!(data_only.any_data());
        assert!(!data_only.uninstall_app);
        assert!(!super::UninstallOptions::default().any_data());
    }

    #[test]
    fn options_deserialize_the_app_flag_and_default_to_false() {
        // `#[serde(default)]` fail-safe: a malformed/legacy IPC payload must
        // never arm the app removal.
        let empty: super::UninstallOptions = serde_json::from_str("{}").unwrap();
        assert!(!empty.uninstall_app && !empty.any_data());
        let legacy: super::UninstallOptions =
            serde_json::from_str(r#"{"models":true,"conversations":true}"#).unwrap();
        assert!(!legacy.uninstall_app, "legacy payload must not uninstall");
        let armed: super::UninstallOptions =
            serde_json::from_str(r#"{"uninstall_app":true}"#).unwrap();
        assert!(armed.uninstall_app && !armed.any_data());
    }

    #[test]
    fn data_summary_always_states_the_app_survives() {
        // 830: the gate that only erases DATA must say so, in both modes.
        let full = super::data_summary(&super::UninstallOptions {
            library: true,
            ..Default::default()
        });
        assert!(
            full.contains("stays installed"),
            "full wipe gate must say the app survives: {full}"
        );
        let partial = super::data_summary(&super::UninstallOptions {
            models: true,
            ..Default::default()
        });
        assert!(
            partial.contains("stays installed"),
            "per-category gate must say the app survives: {partial}"
        );
        assert_eq!(
            super::data_summary(&super::UninstallOptions::default()),
            "• (nothing selected)"
        );
    }

    #[test]
    fn app_removal_summary_names_the_artifact_or_the_reason() {
        let removable = super::AppArtifact::SelfRemovable(std::path::PathBuf::from(
            "/Applications/nexe-app.app",
        ));
        let s = super::app_removal_summary(&removable);
        assert!(s.contains("/Applications/nexe-app.app"));
        let blocked = super::AppArtifact::NotSelfRemovable("packaged install");
        let s = super::app_removal_summary(&blocked);
        assert!(s.contains("CANNOT be removed") && s.contains("packaged install"));
    }

    #[test]
    fn completion_message_tells_the_truth_of_each_half() {
        // 836: the app used to quit silently mid-wipe ("sembla que tomba").
        let ok = super::UninstallReport::default();

        let data_only = super::completion_message(&ok, true, None, None);
        assert!(data_only.contains("Dades esborrades correctament"));
        assert!(
            data_only.contains("stays installed and usable"),
            "830: a data-only wipe must say the app is still there: {data_only}"
        );

        let mut failed = super::UninstallReport::default();
        failed.failures.push("/tmp/x: boom".to_string());
        let msg = super::completion_message(&failed, true, None, None);
        assert!(
            !msg.contains("Dades esborrades correctament"),
            "a failed sweep must not claim success: {msg}"
        );
        assert!(
            msg.contains("/tmp/x: boom"),
            "failures must be named: {msg}"
        );

        let scheduled = super::AppRemovalOutcome::Scheduled(std::path::PathBuf::from(
            "/Applications/nexe-app.app",
        ));
        let msg = super::completion_message(&ok, true, Some(&scheduled), None);
        assert!(msg.contains("/Applications/nexe-app.app"));
        assert!(!msg.contains("stays installed and usable"));

        let skipped = super::AppRemovalOutcome::Skipped("packaged install".to_string());
        let msg = super::completion_message(&ok, false, Some(&skipped), None);
        assert!(msg.contains("NOT removed") && msg.contains("packaged install"));
        assert!(
            !msg.contains("Dades esborrades correctament"),
            "app-only uninstall must not claim a data wipe: {msg}"
        );
    }

    /// 828: the wait MUST sit inside the `for /l` body. The first script put
    /// `& timeout` after the `for`, so 120 tight polls finished while nexe
    /// was still alive and `rmdir` never ran. Compiled on every OS — spawn is
    /// Windows-only, the string is the contract.
    #[test]
    fn deferred_script_waits_inside_the_loop_and_only_then_removes() {
        let target = std::path::Path::new(r"C:\Users\u\AppData\Local\com.nexe.app");
        let script = super::deferred_dir_delete_script(target, 4242);
        assert!(
            script.contains("PID eq 4242"),
            "must watch our pid: {script}"
        );
        assert!(
            script.contains(
                r#"|| (rmdir /s /q "C:\Users\u\AppData\Local\com.nexe.app" & exit /b 0)"#
            ),
            "the removal must be CONDITIONAL on the pid being gone: {script}"
        );
        assert!(
            script.ends_with("ping 127.0.0.1 -n 2 >nul)"),
            "the delay must be the last statement INSIDE the do-body: {script}"
        );
        assert!(
            !script.contains(")) & "),
            "chaining the delay AFTER the for (the original #828 bug) is `)) & …`: {script}"
        );
        assert!(
            !script.contains("timeout"),
            "timeout.exe dies with stdin redirected / CREATE_NO_WINDOW: {script}"
        );
        assert!(
            script.contains("(1,1,60)"),
            "the wait must be bounded (~60s): {script}"
        );
    }

    // ── The Windows hand-off to the NSIS uninstaller (830) ────────────────────
    // A sibling of `self_removal`, which is cfg'd to macOS/Linux — that gate is
    // exactly why Windows had no coverage of the app-removal arm at all.

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    mod external_uninstaller {
        use std::path::{Path, PathBuf};

        /// 828: the EBWebView dir is held by our OWN live WebView2, so it can
        /// only be planned for removal, never removed in the sweep.
        #[cfg(target_os = "windows")]
        #[test]
        fn the_live_webview_cache_is_planned_not_swept() {
            use super::super::plan_deferred_cache_delete;

            // Model a home whose %LOCALAPPDATA%\com.nexe.app really exists.
            let home = std::env::temp_dir().join("nexe-828-home");
            let cache = home
                .join("AppData")
                .join("Local")
                .join("com.nexe.app")
                .join("EBWebView");
            std::fs::create_dir_all(&cache).expect("temp cache");

            let planned = plan_deferred_cache_delete(&home, true);
            let library = home.join("AppData").join("Local").join("com.nexe.app");
            assert_eq!(
                planned,
                Some(library.clone()),
                "a confirmed library wipe must plan the deferred removal"
            );

            // The in-process sweep must leave that dir alone (and not record it
            // as a failure): WebView2 still holds it. data/config can go.
            let data = home.join("Roaming").join("com.nexe.app");
            let config = home.join("Roaming").join("com.nexe.app").join("config");
            std::fs::create_dir_all(&data).expect("temp data");
            std::fs::create_dir_all(&config).expect("temp config");
            let opts = super::super::UninstallOptions {
                library: true,
                ..Default::default()
            };
            let report = super::super::selective_reset_paths(&config, &data, &home, &opts);
            assert!(
                library.exists(),
                "828: the live WebView2 dir is planned, not swept"
            );
            assert!(
                report.failures.iter().all(|f| !f.contains("com.nexe.app")),
                "the closing dialog must not list a deferred dir as a failure: {:?}",
                report.failures
            );

            // Not requested → nothing planned (no surprise deletions).
            assert_eq!(plan_deferred_cache_delete(&home, false), None);

            let _ = std::fs::remove_dir_all(&home);
            // Nothing on disk → nothing to defer.
            assert_eq!(plan_deferred_cache_delete(&home, true), None);
        }

        /// The message must not report as lost something we are about to remove.
        #[cfg(target_os = "windows")]
        #[test]
        fn the_user_is_told_the_cache_goes_on_close() {
            use super::super::{completion_message, UninstallReport};
            let cache = PathBuf::from(r"C:\Users\u\AppData\Local\com.nexe.app");
            let msg = completion_message(&UninstallReport::default(), true, None, Some(&cache));
            assert!(
                msg.contains("com.nexe.app") && msg.contains("in use until nexe closes"),
                "828: the closing message must explain the deferral: {msg}"
            );
        }

        /// 830: before this, Windows had ZERO tests on the app-removal arm —
        /// the modal's text pointer could be changed to a false promise with
        /// the suite fully green.
        #[cfg(target_os = "windows")]
        #[test]
        fn artifact_is_the_uninstaller_next_to_the_exe() {
            use super::super::{app_artifact, AppArtifact};

            // A real install layout: uninstall.exe sits beside the exe. Build
            // it in a temp dir because app_artifact requires the file to exist
            // (a promise to launch a missing binary is exactly the bug).
            let dir = std::env::temp_dir().join("nexe-830-artifact");
            let _ = std::fs::create_dir_all(&dir);
            let uninst = dir.join("uninstall.exe");
            std::fs::write(&uninst, b"stub").expect("temp uninstaller");
            let exe = dir.join("nexe-app.exe");
            assert_eq!(
                app_artifact(&exe, None),
                AppArtifact::ExternalUninstaller(uninst.clone()),
                "ticking the box must hand off to the real uninstaller"
            );

            // No uninstaller (a dev/cargo run) → say so, never promise.
            std::fs::remove_file(&uninst).expect("cleanup");
            assert!(matches!(
                app_artifact(&exe, None),
                AppArtifact::NotSelfRemovable(_)
            ));
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[cfg(target_os = "windows")]
        #[test]
        fn only_a_real_uninstaller_path_passes_the_guard() {
            use super::super::is_safe_external_uninstaller;
            assert!(is_safe_external_uninstaller(Path::new(
                r"C:\Users\u\AppData\Local\nexe-app\uninstall.exe"
            )));
            // Not absolute.
            assert!(!is_safe_external_uninstaller(Path::new(
                r"nexe-app\uninstall.exe"
            )));
            // Traversal.
            assert!(!is_safe_external_uninstaller(Path::new(
                r"C:\Users\u\..\evil\uninstall.exe"
            )));
            // Any other neighbour binary is never launchable.
            assert!(!is_safe_external_uninstaller(Path::new(
                r"C:\Users\u\AppData\Local\nexe-app\nexe-app.exe"
            )));
            // Root-level dropper.
            assert!(!is_safe_external_uninstaller(Path::new(
                r"C:\uninstall.exe"
            )));
        }

        /// The whole point of 830: a hand-off must reach the ARM, not be
        /// silently classified as "nothing to do" like NotSelfRemovable is.
        #[cfg(target_os = "windows")]
        #[test]
        fn a_hand_off_plan_is_not_a_skip() {
            use super::super::{plan_app_removal, AppArtifact, AppRemovalOutcome};
            let u = PathBuf::from(r"C:\Users\u\AppData\Local\nexe-app\uninstall.exe");
            let artifact = AppArtifact::ExternalUninstaller(u.clone());
            assert_eq!(
                plan_app_removal(Some(&artifact)),
                AppRemovalOutcome::HandOff(u)
            );
        }

        /// 830's symptom was the message promising a removal that never
        /// happened. Both user-facing strings must name the uninstaller.
        #[cfg(target_os = "windows")]
        #[test]
        fn the_user_is_told_the_uninstaller_will_open() {
            use super::super::{
                app_removal_summary, completion_message, AppArtifact, AppRemovalOutcome,
                UninstallReport,
            };
            let u = PathBuf::from(r"C:\Users\u\AppData\Local\nexe-app\uninstall.exe");

            let summary = app_removal_summary(&AppArtifact::ExternalUninstaller(u.clone()));
            assert!(
                summary.contains("uninstall.exe"),
                "the gate must name what it will launch: {summary}"
            );
            assert!(
                !summary.contains("CANNOT be removed"),
                "830: it is removable via the uninstaller — the old text lied the other way"
            );

            let msg = completion_message(
                &UninstallReport::default(),
                false,
                Some(&AppRemovalOutcome::HandOff(u)),
                None,
            );
            assert!(
                msg.contains("uninstall.exe") && msg.contains("uninstaller will open"),
                "the closing message must set the expectation: {msg}"
            );
        }
    }

    // ── The self-removal mechanism (only where it exists) ─────────────────────

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    mod self_removal {
        use std::fs;
        use std::path::{Path, PathBuf};
        use tempfile::TempDir;

        /// A target the platform guard accepts, inside a tempdir. NEVER a real
        /// install path: these tests arm a real `rm -rf`.
        fn make_target(tmp: &Path) -> PathBuf {
            let target = tmp.join(format!("nexe-app{}", super::super::APP_ARTIFACT_SUFFIX));
            #[cfg(target_os = "macos")]
            {
                fs::create_dir_all(target.join("Contents").join("MacOS")).unwrap();
                fs::write(target.join("Contents").join("Info.plist"), b"<plist/>").unwrap();
            }
            #[cfg(target_os = "linux")]
            {
                fs::write(&target, b"\x7fELF").unwrap();
            }
            target
        }

        #[test]
        fn guard_accepts_only_a_plausible_artifact() {
            let ok = PathBuf::from(format!(
                "/Applications/nexe-app{}",
                super::super::APP_ARTIFACT_SUFFIX
            ));
            assert!(super::super::is_safe_app_artifact(&ok));
            // Relative → would resolve against the helper's cwd.
            assert!(!super::super::is_safe_app_artifact(Path::new(
                "nexe-app.app"
            )));
            // Traversal.
            assert!(!super::super::is_safe_app_artifact(&PathBuf::from(
                format!(
                    "/Applications/../etc/nexe-app{}",
                    super::super::APP_ARTIFACT_SUFFIX
                )
            )));
            // Wrong (or missing) suffix — a bare dir name can never qualify.
            assert!(!super::super::is_safe_app_artifact(Path::new(
                "/Applications/nexe-app"
            )));
            assert!(!super::super::is_safe_app_artifact(Path::new("/")));
            // Root-level artifact: refused rather than nuked.
            assert!(!super::super::is_safe_app_artifact(&PathBuf::from(
                format!("/nexe-app{}", super::super::APP_ARTIFACT_SUFFIX)
            )));
        }

        #[cfg(target_os = "macos")]
        #[test]
        fn artifact_is_the_nearest_app_bundle() {
            use super::super::{app_artifact, AppArtifact};
            assert_eq!(
                app_artifact(
                    Path::new("/Applications/nexe-app.app/Contents/MacOS/nexe-app"),
                    None
                ),
                AppArtifact::SelfRemovable(PathBuf::from("/Applications/nexe-app.app"))
            );
            // A dev run (cargo target dir) is not a bundle — nothing to delete.
            assert!(matches!(
                app_artifact(Path::new("/Users/x/p/target/debug/nexe-app"), None),
                AppArtifact::NotSelfRemovable(_)
            ));
        }

        #[cfg(target_os = "linux")]
        #[test]
        fn artifact_comes_from_the_appimage_runtime_only() {
            use super::super::{app_artifact, AppArtifact};
            let exe = Path::new("/tmp/.mount_nexeXX/usr/bin/nexe-app");
            assert_eq!(
                app_artifact(exe, Some(Path::new("/home/u/Desktop/nexe-app.AppImage"))),
                AppArtifact::SelfRemovable(PathBuf::from("/home/u/Desktop/nexe-app.AppImage"))
            );
            // A .deb install exports no APPIMAGE → the package manager owns it.
            assert!(matches!(
                app_artifact(Path::new("/usr/bin/nexe-app"), None),
                AppArtifact::NotSelfRemovable(_)
            ));
            // A bogus APPIMAGE value never becomes a removal target.
            assert!(matches!(
                app_artifact(exe, Some(Path::new("/etc"))),
                AppArtifact::NotSelfRemovable(_)
            ));
        }

        #[test]
        fn sh_quote_survives_spaces_and_quotes() {
            assert_eq!(super::super::sh_quote("/A B/x.app"), "'/A B/x.app'");
            assert_eq!(
                super::super::sh_quote("/it's/x.app"),
                "'/it'\\''s/x.app'",
                "an embedded quote must not break out of the literal"
            );
        }

        #[test]
        fn script_waits_for_the_pid_and_only_then_removes() {
            let script = super::super::self_delete_script(Path::new("/A B/nexe.app"), 4242);
            assert!(
                script.contains("kill -0 4242"),
                "must watch our pid: {script}"
            );
            assert!(
                script.contains("kill -0 4242 2>/dev/null || rm -rf -- '/A B/nexe.app'"),
                "the removal must be CONDITIONAL on the pid being gone: {script}"
            );
            assert!(
                script.contains("[ $i -lt 300 ]"),
                "the wait must be bounded: {script}"
            );
        }

        #[test]
        fn spawn_refuses_an_unsafe_target_and_touches_nothing() {
            let tmp = TempDir::new().unwrap();
            let victim = tmp.path().join("not-an-artifact");
            fs::create_dir_all(&victim).unwrap();
            let err = super::super::spawn_self_delete(&victim, std::process::id())
                .expect_err("the guard must refuse a target without the artifact suffix");
            assert!(err.contains("refused"), "{err}");
            assert!(victim.exists(), "nothing may be scheduled for removal");
        }

        #[test]
        fn self_delete_removes_the_target_once_the_pid_is_gone() {
            // The REAL mechanism, end to end, against a tempdir: arm the remover
            // on a throwaway pid, kill it, and watch the artifact disappear.
            let tmp = TempDir::new().unwrap();
            let target = make_target(tmp.path());
            let mut victim_pid = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg("sleep 30")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("sh must be spawnable");
            super::super::spawn_self_delete(&target, victim_pid.id()).expect("armed");
            assert!(target.exists(), "still alive → nothing removed yet");
            victim_pid.kill().unwrap();
            victim_pid.wait().unwrap(); // reap: the pid is now really gone

            let mut gone = false;
            for _ in 0..100 {
                if !target.exists() {
                    gone = true;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            assert!(gone, "the remover must delete {} ", target.display());
        }

        // ── #836: planning the message must never arm the remover ─────────────

        #[test]
        fn planning_removal_does_not_touch_the_filesystem() {
            // The closing dialog names the removal target from `plan_app_removal`
            // — this must be a pure prediction. If it secretly armed the
            // remover, the 60s clock would start while the human is still
            // reading the dialog (the exact #836 race).
            let tmp = TempDir::new().unwrap();
            let target = make_target(tmp.path());
            let artifact = super::super::AppArtifact::SelfRemovable(target.clone());
            let planned = super::super::plan_app_removal(Some(&artifact));
            assert_eq!(
                planned,
                super::super::AppRemovalOutcome::Scheduled(target.clone())
            );
            std::thread::sleep(std::time::Duration::from_millis(500));
            assert!(
                target.exists(),
                "planning must never touch the filesystem or spawn a remover"
            );
        }

        #[test]
        fn planning_with_no_artifact_is_skipped_without_touching_anything() {
            let planned = super::super::plan_app_removal(None);
            assert!(matches!(
                planned,
                super::super::AppRemovalOutcome::Skipped(_)
            ));
        }

        #[test]
        fn arm_app_removal_actually_arms_the_remover() {
            // Same mechanism as self_delete_removes_the_target_once_the_pid_is_gone,
            // but going through arm_app_removal(planned, pid) — the call
            // uninstall_with_options makes AFTER the user's ack.
            let tmp = TempDir::new().unwrap();
            let target = make_target(tmp.path());
            let mut victim_pid = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg("sleep 30")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("sh must be spawnable");

            let planned = super::super::AppRemovalOutcome::Scheduled(target.clone());
            super::super::arm_app_removal(&planned, victim_pid.id());
            assert!(target.exists(), "still alive → nothing removed yet");

            victim_pid.kill().unwrap();
            victim_pid.wait().unwrap();

            let mut gone = false;
            for _ in 0..100 {
                if !target.exists() {
                    gone = true;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            assert!(gone, "arm_app_removal must actually arm the remover");
        }

        #[test]
        fn arm_app_removal_is_a_no_op_for_a_skipped_plan() {
            let tmp = TempDir::new().unwrap();
            let target = make_target(tmp.path());
            let planned = super::super::AppRemovalOutcome::Skipped("not requested".to_string());
            super::super::arm_app_removal(&planned, std::process::id());
            std::thread::sleep(std::time::Duration::from_millis(300));
            assert!(target.exists(), "a Skipped plan must never spawn a remover");
        }
    }
}

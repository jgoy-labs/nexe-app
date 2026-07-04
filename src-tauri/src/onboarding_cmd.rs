//! Onboarding state commands.
//!
//! `check_first_run`          — returns true when the wizard has not been completed.
//! `mark_onboarding_complete` — writes the completion flag to the app config dir.
//!
//! Detection is file-based (not localStorage) so it survives browser storage clears
//! and is consistent across WebView restarts.

use std::sync::atomic::{AtomicBool, Ordering};
use tauri::{AppHandle, Manager};

/// Return `true` when the onboarding wizard has not yet been completed.
///
/// Called via `invoke("check_first_run")` at frontend boot.
/// The flag file lives at `<app_config_dir>/onboarding_complete`.
#[tauri::command]
pub fn check_first_run(app: AppHandle) -> bool {
    let flag = app
        .path()
        .app_config_dir()
        .unwrap_or_default()
        .join("onboarding_complete");
    !flag.exists()
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
#[tauri::command]
pub fn reset_installation(app: AppHandle) {
    // Errors are silently ignored — worst case the wizard shows again, which
    // is a safe degradation for a non-destructive reset.
    let _ = reset_installation_inner(&app, false);
    // The reset just removed `.extracted`, so the boot snapshot is stale by
    // design: clear it so the banner does not reappear after the frontend's
    // `location.reload()`.
    if let Some(snapshot) = app.try_state::<PartialInstallAtBoot>() {
        snapshot.0.store(false, Ordering::Relaxed);
    }
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

fn remove_dir_tracked(report: &mut UninstallReport, path: &std::path::Path) {
    match std::fs::remove_dir_all(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => report.failures.push(format!("{}: {e}", path.display())),
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
    remove_file_tracked(&mut report, &config_dir.join("onboarding_complete"));
    // NB: the sidecar persists its state as `onboarding.json` under
    // NEXE_DATA_DIR (= <app_data_dir>/sidecar/data). The old name
    // `onboarding_state.json` never existed on disk — the reset was
    // silently skipping the sidecar state.
    remove_file_tracked(
        &mut report,
        &data_dir
            .join("sidecar")
            .join("data")
            .join("onboarding.json"),
    );
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
}

//! nexe-app library crate — Tauri v2 desktop shell. Phase 1. CSP fix.
//!
//! Entry point and Builder setup. The logic lives in modules:
//! - [`auth`] — session token UUID v4 + fetch_from_sidecar Bearer proxy
//! - [`catalog`] — model catalog Tauri command (remote + embedded fallback)
//! - [`handler`] — plugin:// URI scheme handler + threadpool + reentrancy + queue infra
//! - [`hardware`] — hardware detection Tauri command (RAM, OS, disk)
//! - [`integrity`] — SHA-256 plugin integrity, re-hash per-request (C01) + LRU observability
//! - [`lifecycle`] — graceful_quit + quit_app command
//! - [`onboarding_cmd`] — first-run detection + completion flag commands
//! - [`rate_limit`] — token bucket per-plugin + LRU cap
//! - [`sidecar`] — state types (SidecarPort/HttpClient/SidecarChild) + path resolvers
//! - [`validate`] — plugin_id + request + path traversal

pub mod auth;
pub mod catalog;
pub mod handler;
pub mod hardware;
pub mod integrity;
pub mod lifecycle;
pub(crate) mod logging;
pub mod onboarding_cmd;
pub mod rate_limit;
pub mod sidecar;
pub mod sidecar_extract;
#[cfg(test)]
mod test_hygiene;
pub mod validate;
#[cfg(windows)]
pub mod win_job;

// Re-export command functions under their short names so
// generate_handler! registers them as "get_hardware" etc. (matching
// the isolation.js allowlist and frontend invoke() calls).
use catalog::fetch_catalog;
use hardware::get_hardware;
use onboarding_cmd::{
    check_first_run, check_partial_install, mark_onboarding_complete, reset_installation,
    uninstall_with_options,
};

/// Host allowlist for [`open_external_url`]. Only origins the app legitimately
/// links to reach the system browser; everything else is rejected. A host
/// matches when it equals an apex exactly OR is a subdomain of it.
const EXTERNAL_URL_ALLOWED_HOSTS: &[&str] = &["server-nexe.com", "huggingface.co"];

/// Validate an external URL before dispatching it to the OS browser handler.
///
/// WSA-004 / WSD-003 / WSE-002: parse with `url::Url::parse` (scheme + host),
/// NOT a `starts_with("http")` prefix. A prefix check accepts any origin
/// (`https://evil.example/phish`), letting the webview drive the system browser
/// to an attacker-chosen page (CWE-601 phishing). Here the scheme must be
/// `http`/`https` AND the host must be on [`EXTERNAL_URL_ALLOWED_HOSTS`].
fn is_allowed_external_url(url: &str) -> bool {
    let parsed = match url::Url::parse(url) {
        Ok(u) => u,
        Err(_) => return false,
    };
    if !matches!(parsed.scheme(), "http" | "https") {
        return false;
    }
    let Some(host) = parsed.host_str() else {
        return false; // opaque / hostless URL — reject
    };
    let host = host.to_ascii_lowercase();
    EXTERNAL_URL_ALLOWED_HOSTS
        .iter()
        .any(|apex| host == *apex || host.ends_with(&format!(".{apex}")))
}

/// Open an external http/https URL in the system default browser.
///
/// Tauri v2 blocks `target="_blank"` and `window.open()` for external URLs
/// by default. This command calls the OS handler (`open` on macOS, `xdg-open`
/// on Linux) so the system browser receives the URL instead of the webview.
///
/// Only allowlisted http/https origins are accepted (see
/// [`is_allowed_external_url`]) — every other scheme or host is rejected and
/// logged, closing the arbitrary-origin navigation / phishing gap.
#[tauri::command]
fn open_external_url(url: String) {
    if !is_allowed_external_url(&url) {
        tracing::warn!(url, "open_external_url: rejected (scheme/host not allowed)");
        return;
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(&url).spawn();
    }
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("xdg-open").arg(&url).spawn();
    }
    tracing::info!(url, "open_external_url: dispatched to system browser");
}

// Public re-exports for external API and lifecycle/auth compatibility
// (`crate::SidecarPort`, `crate::HttpClient`, `crate::SidecarChild` still work).
pub use auth::{ApiKey, AuthToken};
pub use integrity::compute_plugin_hash;
pub use sidecar::{HttpClient, SidecarChild, SidecarPort};

// Internal re-exports to facilitate use from `mod tests` in lib.rs.
#[cfg(test)]
pub(crate) use handler::{content_type_for, finish_with_timing, HANDLER_DEPTH, MAX_HANDLER_DEPTH};
#[cfg(test)]
#[allow(unused_imports)]
pub(crate) use integrity::{verified_plugins, verify_plugin_integrity};
#[cfg(test)]
pub(crate) use lifecycle::dialog_try_acquire_in;
#[cfg(test)]
pub(crate) use rate_limit::{rate_limiters, RATE_LIMIT_LRU_CAP};
#[cfg(test)]
pub(crate) use validate::{resolve_plugin_path, validate_plugin_id};

use crate::auth::fetch_from_sidecar;
use crate::handler::{
    err_response, extract_plugin_id_from_uri, handler_pool, plugin_protocol_handler,
    try_acquire_pending_slot, MAX_QUEUED, PENDING_COUNT,
};
use crate::lifecycle::{graceful_quit, quit_app, EXIT_CONFIRMED};
use crate::rate_limit::plugin_rate_limits_ok;
use crate::sidecar::{
    reserve_ephemeral_port, resolve_sidecar_path, restart_try_acquire, verify_port_free,
    RestartGuard, SidecarLogPath, SpawnContext,
};
use crate::validate::validate_request;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::Mutex;
use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Manager, UriSchemeContext, WindowEvent,
};

// example #[tauri::command] — end-to-end Rust ↔ JS pattern.
// Called from `src/api/commands.js` via `invoke("greet", { name })`.
#[tauri::command]
fn greet(name: &str) -> String {
    format!("Hello, {name}! Greeted from Rust.")
}

/// Returns the dynamic sidecar port assigned at startup (or after restart).
/// Frontend uses this to build sidecar URLs instead of a hardcoded constant.
///
/// Lock-free read via `SidecarPort::get` — the underlying `AtomicU16`
/// can be updated by `restart_sidecar` without disturbing concurrent readers.
#[tauri::command]
fn get_sidecar_port(port_state: tauri::State<'_, SidecarPort>) -> u16 {
    port_state.get()
}

/// B3 (Windows port, option c — validated externally 2026-06-11): build the
/// sidecar Command without the bash launcher. Windows has no bash, and no
/// exec(2) to make a wrapper transparent — an intermediate cmd/bat parent
/// would break the NEXE_TRAY_PID watchdog and sit outside the Job Object
/// story. So the Rust spawner replicates `nexe-sidecar` inline:
///   - python-runtime\python.exe (bundled PBS) spawned directly with the uvicorn argv
///   - PYTHONNOUSERSITE/PYTHONDONTWRITEBYTECODE here (PYTHONUNBUFFERED is set
///     by the shared spawn code), PYTHONHOME always the bundled python-runtime
///     (the boot interpreter lives inside it, so it always exists)
///   - fastembed cache seeded at first launch (launcher Step 5.9 equivalent)
///   - auth token via child env NEXE_TOKEN_INTERNAL: equivalent threat model
///     to the POSIX stdin→export (reading another same-user process env needs
///     PROCESS_VM_READ, just like /proc/<pid>/environ)
///   - CREATE_NO_WINDOW so no console flashes in GUI mode
///
/// `sidecar_dir` resolution mirrors the launcher: NEXE_SIDECAR_DIR equivalent
/// (the extraction dir, prod) or the launcher's own directory (dev bench).
#[cfg(windows)]
fn build_windows_sidecar_command(
    sidecar_path: &Path,
    sidecar_data_dir: Option<&std::path::Path>,
    auth_token: &str,
    port: u16,
) -> Result<Command, String> {
    use std::os::windows::process::CommandExt as _;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let sidecar_dir = match sidecar_data_dir {
        Some(dir) => dir.to_path_buf(),
        None => sidecar_path
            .parent()
            .ok_or("sidecar path has no parent")?
            .to_path_buf(),
    };
    // Spawn the bundled PBS python.exe DIRECTLY (not venv\Scripts\python.exe). The venv
    // launcher is a redirector that resolves pyvenv.cfg `home` relative to itself and does
    // NOT support a relocatable relative home on Windows (fails "No Python at ..." once the
    // bundle moves out of the build dir). The PBS runs standalone; the venv's installed
    // packages are exposed to it via a relative .pth that build-sidecar.sh writes into the
    // PBS site-packages (Step 5.5b). Empirically validated on Win11 ARM64 (2026-07-01).
    let python = sidecar_dir.join("python-runtime").join("python.exe");
    if !python.is_file() {
        return Err(format!(
            "sidecar python does not exist: {} — build the Windows sidecar bundle",
            python.display()
        ));
    }
    let app_dir = sidecar_dir.join("app");

    seed_fastembed_cache(&sidecar_dir);

    let mut cmd = Command::new(python);
    cmd.args([
        "-m",
        "uvicorn",
        "core.app:app",
        "--host",
        "127.0.0.1",
        "--port",
        &port.to_string(),
        "--workers",
        "1",
        "--lifespan",
        "on",
        "--no-access-log",
        "--app-dir",
        &app_dir.to_string_lossy(),
    ]);
    cmd.env("PYTHONNOUSERSITE", "1")
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env("NEXE_TOKEN_INTERNAL", auth_token);
    // The boot interpreter lives inside python-runtime/ (verified above via `python`),
    // so PYTHONHOME is always the bundled runtime — no conditional needed under the
    // direct-PBS boot. (The old `is_dir()` guard was dead code from the venv-boot era.)
    cmd.env("PYTHONHOME", sidecar_dir.join("python-runtime"));
    // Windows stdio inherits the system ANSI code page (cp1252 on Latin locales);
    // any Unicode print() from the sidecar would raise UnicodeEncodeError and abort
    // the process. PYTHONUTF8=1 forces Python's UTF-8 mode: stdout/stderr AND default
    // file I/O become UTF-8 with the forgiving surrogateescape error handler (so even a
    // lone surrogate from os.fsdecode() round-trips instead of crashing). We deliberately
    // do NOT also set PYTHONIOENCODING=utf-8: that forces the 'strict' handler and would
    // reintroduce the exact crash class. macOS/Linux are already UTF-8 and this fn is
    // #[cfg(windows)], so the fix is scoped to the platform that needs it.
    cmd.env("PYTHONUTF8", "1");
    cmd.creation_flags(CREATE_NO_WINDOW);
    Ok(cmd)
}

/// Launcher Step 5.9 equivalent (Windows): seed the fastembed model cache to
/// the user cache dir at first launch. The bundle ships it read-only inside
/// the app; fastembed writes files_metadata.json on first load, so it must
/// live somewhere writable. Best-effort: a failed seed only means a download
/// at first chat.
#[cfg(windows)]
fn seed_fastembed_cache(sidecar_dir: &Path) {
    let Some(home) = dirs::home_dir() else {
        return;
    };
    let src = sidecar_dir.join("app").join(".fastembed_cache");
    let dst_root = home.join(".cache").join("fastembed");
    // The marker used to be `models--sentence-transformers--<model>`, but
    // fastembed materialises the cache under `models--xenova--<model>` (the
    // ONNX mirror it downloads from). That path never existed, so "first
    // launch" was true on every launch and this re-copied ~1 GB each time.
    //
    // The sentinel carries the IDENTITY of what was seeded, not just the fact
    // that something was: a bare existence check would make the seed a one-shot
    // for the life of the machine, so a later build shipping a different
    // embedder would never install it and the user would silently keep the old
    // one. Kept in lockstep with the POSIX launcher's guard in build-sidecar.sh.
    let sentinel = dst_root.join(".nexe-seeded");
    if !src.is_dir() {
        return;
    }
    let want_id =
        std::fs::read_to_string(src.join(".nexe-embedder-id")).unwrap_or_else(|_| "unknown".into());
    let have_id = std::fs::read_to_string(&sentinel).unwrap_or_default();
    if want_id.trim() == have_id.trim() && !have_id.trim().is_empty() {
        return;
    }
    match copy_dir_recursive(&src, &dst_root) {
        Ok(()) => {
            let _ = std::fs::write(&sentinel, want_id.trim().as_bytes());
            tracing::info!(id = %want_id.trim(), "fastembed cache seeded to user cache");
        }
        Err(e) => {
            tracing::warn!(error = %e, "fastembed cache seed failed (will download at first chat)")
        }
    }
}

/// Minimal recursive copy (no symlink handling — the fastembed cache is plain
/// files; the Windows bundle never ships symlinks, unlike the macOS venv).
#[cfg(windows)]
fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let target = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

#[cfg(all(test, windows))]
mod win_spawn_tests {
    use super::*;
    use std::ffi::OsStr;

    fn make_sidecar_dir() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().expect("tempdir");
        // Windows bundle: the PBS at python-runtime\python.exe is the boot interpreter
        // (spawned directly; the venv launcher can't resolve a relocatable relative home).
        let runtime = tmp.path().join("python-runtime");
        std::fs::create_dir_all(&runtime).unwrap();
        std::fs::write(runtime.join("python.exe"), b"stub").unwrap();
        // The venv still ships (deps reached via a relative .pth) but is not the boot exe.
        std::fs::create_dir_all(tmp.path().join("venv").join("Lib").join("site-packages")).unwrap();
        std::fs::create_dir_all(tmp.path().join("app")).unwrap();
        tmp
    }

    fn env_of(cmd: &Command, key: &str) -> Option<String> {
        cmd.get_envs()
            .find(|(k, _)| *k == OsStr::new(key))
            .and_then(|(_, v)| v.map(|v| v.to_string_lossy().into_owned()))
    }

    #[test]
    fn missing_python_yields_actionable_error() {
        let tmp = tempfile::tempdir().unwrap();
        let err = build_windows_sidecar_command(
            &tmp.path().join("nexe-sidecar"),
            Some(tmp.path()),
            "tok",
            8765,
        )
        .unwrap_err();
        assert!(
            err.contains("python.exe"),
            "err should name the path: {err}"
        );
    }

    #[test]
    fn happy_path_builds_uvicorn_argv_and_token_env() {
        let tmp = make_sidecar_dir();
        let cmd = build_windows_sidecar_command(
            &tmp.path().join("unused-launcher"),
            Some(tmp.path()),
            "tok-123",
            9123,
        )
        .expect("command");

        let program = cmd.get_program().to_string_lossy().into_owned();
        assert!(program.ends_with("python.exe"), "program: {program}");
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(args.windows(2).any(|w| w[0] == "--port" && w[1] == "9123"));
        assert!(args.iter().any(|a| a == "core.app:app"));
        assert!(args.iter().any(|a| a == "--no-access-log"));
        assert_eq!(
            env_of(&cmd, "NEXE_TOKEN_INTERNAL").as_deref(),
            Some("tok-123")
        );
        // The PBS is the boot interpreter, so PYTHONHOME points at python-runtime.
        let home = env_of(&cmd, "PYTHONHOME").expect("PYTHONHOME present");
        assert!(home.ends_with("python-runtime"), "home: {home}");
        // cp1252 guard: the Windows sidecar stdio is forced to UTF-8 so a Unicode
        // print() can't crash the process. PYTHONUTF8=1 alone (its surrogateescape
        // handler); NOT PYTHONIOENCODING=utf-8, whose 'strict' handler would
        // reintroduce the crash — assert it stays unset. See build-fn comment.
        assert_eq!(env_of(&cmd, "PYTHONUTF8").as_deref(), Some("1"));
        assert_eq!(env_of(&cmd, "PYTHONIOENCODING"), None);
    }

    #[test]
    fn boots_pbs_directly_not_venv_launcher() {
        // The venv launcher (Scripts\python.exe) can't resolve a relocatable relative
        // pyvenv.cfg home on Windows, so we spawn python-runtime\python.exe directly and
        // reach the venv deps via a relative .pth. Guard the choice against regressions.
        let tmp = make_sidecar_dir();
        let cmd = build_windows_sidecar_command(
            &tmp.path().join("unused-launcher"),
            Some(tmp.path()),
            "tok",
            8765,
        )
        .expect("command");
        let program = cmd.get_program().to_string_lossy().replace('\\', "/");
        assert!(
            program.ends_with("python-runtime/python.exe"),
            "must boot the PBS directly, got: {program}"
        );
        assert!(
            !program.contains("venv/Scripts"),
            "must NOT boot the venv launcher: {program}"
        );
    }

    #[test]
    fn copy_dir_recursive_copies_nested_tree_and_is_idempotent() {
        let src = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(src.path().join("a").join("b")).unwrap();
        std::fs::write(src.path().join("a").join("b").join("f.txt"), b"x").unwrap();
        std::fs::write(src.path().join("root.txt"), b"y").unwrap();
        let dst = tempfile::tempdir().unwrap();
        let dst_root = dst.path().join("out");

        copy_dir_recursive(src.path(), &dst_root).expect("first copy");
        assert_eq!(
            std::fs::read(dst_root.join("a").join("b").join("f.txt")).unwrap(),
            b"x"
        );
        assert_eq!(std::fs::read(dst_root.join("root.txt")).unwrap(), b"y");

        // Partially/fully existing destination must not fail (re-seed path).
        copy_dir_recursive(src.path(), &dst_root).expect("second copy over existing");
    }

    #[test]
    fn dev_fallback_resolves_dir_from_launcher_parent() {
        let tmp = make_sidecar_dir();
        // sidecar_data_dir = None (dev): the dir is the launcher's parent.
        let cmd =
            build_windows_sidecar_command(&tmp.path().join("nexe-sidecar"), None, "tok", 8765)
                .expect("command");
        let program = cmd.get_program().to_string_lossy().into_owned();
        assert!(program.starts_with(&tmp.path().to_string_lossy().into_owned()));
    }
}

/// Spawn the sidecar process with auth token via stdin.
///
/// N1: NEXE_SIDECAR=1 signals server-nexe to NOT kill processes on port conflict.
/// N3: token is UUID v4 session key, not JWT. Written to stdin to avoid
/// /proc/<pid>/environ leak. (Windows: token goes via child env instead —
/// see `build_windows_sidecar_command`.)
fn spawn_sidecar_process(
    sidecar_path: &Path,
    auth_token: &str,
    port: u16,
    sidecar_data_dir: Option<&std::path::Path>,
    api_key: &str,
    stdout_log_path: Option<&std::path::Path>,
) -> Result<std::process::Child, String> {
    // Ensure the sidecar_data_dir subfolders exist before
    // the spawn (logs, data, cache, vectors) — runner.py expects them.
    if let Some(dir) = sidecar_data_dir {
        for sub in &["logs", "data", "cache", "vectors"] {
            let p = dir.join(sub);
            if !p.exists() {
                std::fs::create_dir_all(&p).ok();
            }
        }
    }

    // POSIX: the launcher script (nexe-sidecar, bash) resolves venv/app and
    // reads the auth token from stdin. Windows (B3): no bash and no exec(2),
    // so the venv python.exe is spawned directly and this function replicates
    // the launcher's work inline (env, fastembed seed, token via child env).
    #[cfg(not(windows))]
    let mut cmd = Command::new(sidecar_path);
    #[cfg(windows)]
    let mut cmd = build_windows_sidecar_command(sidecar_path, sidecar_data_dir, auth_token, port)?;

    // macOS app bundles launch with a minimal PATH that omits /usr/local/bin
    // and /opt/homebrew/bin, so shutil.which("ollama") fails inside the sidecar
    // even when Ollama is installed. Prepend the standard tool locations so
    // Python can resolve externally-installed binaries (Ollama, ffmpeg, etc.).
    // (Unix-only: these ':'-joined prefixes would corrupt the ';'-separated
    // Windows PATH, and Windows inherits a complete PATH anyway.)
    #[cfg(not(windows))]
    {
        let base_path = std::env::var("PATH").unwrap_or_default();
        let augmented_path = format!("/usr/local/bin:/opt/homebrew/bin:/opt/local/bin:{base_path}");
        cmd.env("PATH", augmented_path);
    }
    cmd.env("NEXE_PORT", port.to_string())
        .env("NEXE_SERVER_PORT", port.to_string())   // server-nexe alias
        .env("NEXE_HOST", "127.0.0.1")
        .env("NEXE_SIDECAR", "1")
        .env("NEXE_ENV", "production")               // força production
        .env("NEXE_AUTO_INGEST_KNOWLEDGE", "1")
        .env("NEXE_PRIMARY_API_KEY", api_key)        // nom correcte (era NEXE_API_KEY)
        .env("NEXE_TRAY_PID", std::process::id().to_string())   // evita doble tray + watchdog
        .env("NEXE_PARENT_PID", std::process::id().to_string()) // watchdog parent
        .env(
            "NEXE_APPROVED_MODULES",
            "security,memory,rag,embeddings,mlx_module,llama_cpp_module,ollama_module,web_ui_module",
        )
        // web_ui_module enabled to expose /ui/* endpoints (info/backends/
        // sessions/chat) AND the web UI itself: the Tauri webview navigates to
        // the sidecar's loopback http://127.0.0.1:{port}/ui/ (revert 2026-05-21;
        // ADR-0021 supersedes ADR-0004's "no UI via localhost").
        // Net env contamination — scrub inherited env vars
        .env_remove("NEXE_AUTH_TOKEN")
        .env_remove("NEXE_ADMIN_API_KEY")            // prevents an env var inherited from the environment from overwriting the primary key
        .env_remove("NEXE_DEV_MODE")
        .env_remove("NEXE_DEV_MODE_ALLOW_REMOTE")
        .env_remove("PYTHONPATH")
        .env_remove("VIRTUAL_ENV")
        .env_remove("DYLD_LIBRARY_PATH")
        .env_remove("DYLD_FALLBACK_LIBRARY_PATH")
        // Linux portability: Linux equivalents of the macOS DYLD_*
        // scrub. If the user has LD_LIBRARY_PATH/LD_PRELOAD/LD_AUDIT in the
        // shell that launches the AppImage, the Python sidecar would inherit
        // arbitrary .so injection (classic glibc hooking vector). Defensive
        // cross-platform: env_remove is a no-op if the var is not set, with no
        // cost on macOS.
        .env_remove("LD_LIBRARY_PATH")
        .env_remove("LD_PRELOAD")
        .env_remove("LD_AUDIT")
        // Tray logs viewer step: capture all sidecar output — including
        // pre-logger crashes (import error, `.so` blocked by Gatekeeper,
        // segfault). Without this, in a production `.app` stdout/stderr go
        // nowhere, and a sidecar dying at the first instant leaves the
        // frontend hung on retry-poll of `/health/ready` with no trace on
        // disk. Observed on the laptop 2026-05-18 (~320s retry-poll, no logs).
        .env("PYTHONUNBUFFERED", "1")
        .stdin(Stdio::piped());
    if let Some(log_path) = stdout_log_path {
        // Simple rotation — if the previous log exceeds 10 MB, archive it as
        // `.old`. Avoids unbounded growth across runs without external help.
        if let Ok(meta) = std::fs::metadata(log_path) {
            if meta.len() > 10 * 1024 * 1024 {
                let old = log_path.with_extension("log.old");
                let _ = std::fs::rename(log_path, &old);
            }
        }
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)
        {
            Ok(stdout_file) => match stdout_file.try_clone() {
                Ok(stderr_file) => {
                    cmd.stdout(Stdio::from(stdout_file));
                    cmd.stderr(Stdio::from(stderr_file));
                    tracing::info!(path = %log_path.display(), "sidecar stdout/stderr captured to file");
                }
                Err(e) => {
                    tracing::warn!(error = %e, "sidecar log try_clone failed — stderr inherits");
                    cmd.stdout(Stdio::from(stdout_file));
                }
            },
            Err(e) => {
                tracing::warn!(path = %log_path.display(), error = %e, "sidecar log open failed — stdout inherits");
            }
        }
    }
    if let Some(dir) = sidecar_data_dir {
        cmd.env("NEXE_SIDECAR_DIR", dir);
        cmd.env("NEXE_HOME", dir.join("app").to_string_lossy().to_string());
        cmd.env(
            "NEXE_LOGS_DIR",
            dir.join("logs").to_string_lossy().to_string(),
        );
        cmd.env(
            "NEXE_DATA_DIR",
            dir.join("data").to_string_lossy().to_string(),
        );
        cmd.env(
            "NEXE_CACHE_DIR",
            dir.join("cache").to_string_lossy().to_string(),
        );
        cmd.env(
            "NEXE_QDRANT_PATH",
            dir.join("vectors").to_string_lossy().to_string(),
        );
        // Pin cwd to sidecar app dir. Without this, the Python child inherits
        // the cwd of the Tauri parent (which in production can be any folder
        // depending on how the app is launched). The module manager's
        // `_find_initial_config` calls `validate_safe_path(config_path, Path.cwd())`
        // and if the cwd is arbitrary, it rejects the production path and falls into
        // a fallback that resolves modules with absolute paths, breaking plugin loading.
        // Pinning cwd = NEXE_HOME guarantees stable paths.
        cmd.current_dir(dir.join("app"));
    }
    // Propagate user models dir if ~/models/ exists.
    // mlx_module and llama_cpp_module call get_models_dir() which honours
    // NEXE_STORAGE_PATH first, then NEXE_DATA_DIR/models, then cwd fallback.
    // Without this propagation, a fresh install has no models in the bundle
    // storage and the dropdowns are empty until the user manually copies or
    // symlinks them.
    if let Some(home) = dirs::home_dir() {
        let user_models = home.join("models");
        if user_models.exists() && user_models.is_dir() {
            cmd.env(
                "NEXE_STORAGE_PATH",
                user_models.to_string_lossy().to_string(),
            );
        }
    }
    // Disable hf_xet COMPLETELY at sidecar spawn. HF Hub
    // environment variables are read at huggingface_hub import time
    // (documented at https://huggingface.co/docs/huggingface_hub/package_reference/environment_variables);
    // setting them post-import inside the worker thread (the earlier
    // attempt) is silently ignored. The Rust spawn is the only place we
    // can set them BEFORE Python imports HF, so it is.
    //
    // Why disable rather than enable HIGH_PERFORMANCE: the previous
    // strategy (HIGH_PERFORMANCE for RAM >= 32 GB) caused stalled
    // downloads of model.safetensors on Apple Silicon / 128 GB (empíric
    // 2026-05-20). hf_xet deadlocks silently mid-transfer; configs and
    // tokenizers download via httpx fine, but the big safetensors file
    // hangs at 0%. Same family of issues as upstream #800 (GCP) and #446
    // (Windows). Reproduït i validat empíricament.
    //
    // Performance trade-off: httpx fallback ~50-100 MB/s vs xet teòric
    // ~200 MB/s — for a 2.5 GB model that's 25-50 s vs 12-25 s. Acceptable
    // for onboarding UX (< 5 min objective) and the only reliable path
    // until hf_xet's macOS ARM64 transfer engine stabilises.
    cmd.env("HF_HUB_DISABLE_XET", "1");
    tracing::info!("hf_xet disabled at sidecar spawn (HF_HUB_DISABLE_XET=1)");
    #[cfg(unix)]
    cmd.process_group(0);
    let mut child = cmd.spawn().map_err(|e| {
        tracing::error!(error = %e, "sidecar spawn failed");
        format!("sidecar spawn: {e}")
    })?;
    // K-002: Windows counterpart of process_group(0). Ties the sidecar tree
    // (python + grandchildren) to this process via a KILL_ON_JOB_CLOSE Job
    // Object, so a Tauri crash can no longer leave orphans. The window
    // between spawn() and the assignment is accepted for now: Python takes
    // hundreds of ms before it can fork grandchildren (CREATE_SUSPENDED
    // refinement deferred to the release hardening pass).
    #[cfg(windows)]
    {
        if win_job::assign_to_sidecar_job(&child) {
            tracing::info!(
                pid = child.id(),
                "sidecar assigned to KILL_ON_JOB_CLOSE job (K-002)"
            );
        } else {
            tracing::warn!(
                pid = child.id(),
                "Job Object unavailable — relying on taskkill /T fallback only (K-002 partial)"
            );
        }
    }
    if let Some(mut stdin) = child.stdin.take() {
        if let Err(e) = stdin.write_all(format!("{auth_token}\n").as_bytes()) {
            tracing::warn!(error = %e, "sidecar stdin write_all failed");
        }
        drop(stdin);
    }
    Ok(child)
}

/// Splash health-poll budget (seconds). Must not be smaller than the sidecar's
/// own startup deadline (sidecar_extract = 120s) — otherwise the splash gives
/// up and navigates while the sidecar is still legitimately booting (B169).
pub(crate) const HEALTH_POLL_TIMEOUT_SECS: u64 = 120;

/// What `poll_sidecar_health` must do once the poll loop finishes (WSH-005).
///
/// Extracted as a pure decision so the timeout/no-navigate rule is unit
/// testable without a running webview.
#[derive(Debug, PartialEq, Eq)]
enum PostPollAction {
    /// Sidecar healthy, normal run → navigate the webview to the UI.
    Navigate,
    /// Sidecar healthy but first run → the onboarding wizard owns the screen
    /// and navigates itself; do nothing.
    DeferToWizard,
    /// Timeout on a normal run → do NOT navigate (the old "navigating anyway"
    /// path landed the user on a connection-refused page). Stay on the splash
    /// and emit `sidecar-timeout` so the splash JS shows the error + retry.
    StayAndNotify,
    /// Timeout during first run → the wizard is visible and the sidecar may
    /// legitimately not be configured yet; stay quiet, no event.
    StayQuiet,
}

fn post_poll_action(ready: bool, first_run: bool) -> PostPollAction {
    match (ready, first_run) {
        (true, false) => PostPollAction::Navigate,
        (true, true) => PostPollAction::DeferToWizard,
        (false, false) => PostPollAction::StayAndNotify,
        (false, true) => PostPollAction::StayQuiet,
    }
}

/// Poll sidecar health endpoint and navigate to web UI when ready.
///
/// After reverting to the sidecar-served UI, the sidecar serves the full UI again, so we navigate
/// the webview directly to `http://127.0.0.1:{port}/?nexe_api_key={key}`.
/// app.js reads the query param on first load, persists it to the sidecar-
/// origin localStorage, and scrubs the URL via `history.replaceState`.
async fn poll_sidecar_health(
    app_handle: tauri::AppHandle,
    port: u16,
    auth_token: String,
    api_key: String,
    client: reqwest::Client,
) {
    // Actual endpoint exposed by server-nexe (system.py:246). The previous
    // `/api/v1/system/health` did not exist → 30s fallback timeout on first
    // startup.
    let health_url = format!("http://127.0.0.1:{port}/admin/system/health");
    let bearer = format!("Bearer {auth_token}");
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_secs(HEALTH_POLL_TIMEOUT_SECS);
    let mut elapsed = 0u32;
    // WSH-005: `ready` distinguishes success from timeout. The old code used
    // the same `break` for both and then navigated "anyway" — on timeout that
    // replaced the splash with a connection-refused page.
    let mut ready = false;
    loop {
        if std::time::Instant::now() > deadline {
            tracing::warn!(
                "splash: sidecar health timeout after {HEALTH_POLL_TIMEOUT_SECS}s — staying on splash (WSH-005)"
            );
            break;
        }
        match client
            .get(&health_url)
            .header("Authorization", &bearer)
            .timeout(std::time::Duration::from_millis(500))
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => {
                tracing::info!(port, elapsed_s = elapsed / 2, "splash: sidecar ready");
                ready = true;
                break;
            }
            _ => {
                tauri::async_runtime::spawn_blocking(|| {
                    std::thread::sleep(std::time::Duration::from_millis(500));
                })
                .await
                .ok();
                elapsed += 1;
            }
        }
    }
    // If this is a first-run session the onboarding wizard is visible and
    // will navigate to the main UI itself (the api_key arrives via the
    // /installer/finalize response, no separate Tauri command needed).
    // Skip auto-navigation so the health-poll does not clobber the wizard mid-flow.
    // Finding B: share the SAME two-store, self-healing definition as
    // `check_first_run` (flag OR sidecar onboarding.json) so the poll and the
    // frontend never disagree about whether this is a first run — the old
    // flag-only check said "first run" even when the sidecar had finalized,
    // deferring navigation to a wizard that had already handed off.
    let first_run = !crate::onboarding_cmd::is_onboarding_complete(&app_handle);
    match post_poll_action(ready, first_run) {
        PostPollAction::Navigate => {} // fall through to the navigation below
        PostPollAction::DeferToWizard => {
            tracing::info!(
                port,
                "sidecar ready (first-run) — deferring navigation to onboarding wizard"
            );
            return;
        }
        PostPollAction::StayAndNotify => {
            // Tell the splash JS to show its error + retry button now.
            // Timeout coherence: this fires at HEALTH_POLL_TIMEOUT_SECS (120s)
            // and the splash's own fallback timer (main.js HEALTH_TIMEOUT_MS =
            // 120_000 ms) starts at DOMContentLoaded — i.e. never earlier than
            // this task — so even if the event were lost the JS still surfaces
            // its own timeout error; the event just makes it immediate and
            // adds the retry affordance.
            use tauri::Emitter;
            if let Some(w) = app_handle.get_webview_window("main") {
                match w.emit("sidecar-timeout", HEALTH_POLL_TIMEOUT_SECS) {
                    Ok(()) => tracing::info!(port, "splash: emitted sidecar-timeout to splash JS"),
                    Err(e) => {
                        tracing::error!(error = %e, port, "splash: emit sidecar-timeout failed")
                    }
                }
            }
            return;
        }
        PostPollAction::StayQuiet => {
            tracing::warn!(
                port,
                "sidecar not healthy after budget during first run — leaving the wizard in charge"
            );
            return;
        }
    }

    if let Some(w) = app_handle.get_webview_window("main") {
        // Revert (2026-05-21): navigate straight to the sidecar HTTP
        // origin. The previous tauri://localhost/ui/index.html target loaded
        // a stale local copy that drifted from the canonical plugin UI; now
        // the sidecar serves the canonical HTML with all server-side
        // substitutions applied (NEXE_VERSION, data-nexe-lang).
        //
        // localStorage handoff: tauri://localhost (splash) and
        // http://127.0.0.1:{port} (UI) are different origins, so the splash's
        // localStorage isn't visible here. We pass the api_key in the URL
        // fragment (#nexe_api_key=…); fragments are never sent to the server,
        // so the key never reaches uvicorn's access log / support log (K-001).
        // app.js reads it on first load, persists it into the sidecar-origin
        // localStorage, and scrubs the fragment via history.replaceState.
        // UUIDv4 keys ([0-9a-f-]) are safe in URLs; url-encoding guards
        // against future format changes.
        let encoded_key =
            percent_encoding::utf8_percent_encode(&api_key, percent_encoding::NON_ALPHANUMERIC)
                .to_string();
        // The canonical UI is mounted under the `/ui/` prefix by the
        // web_ui_module router (routes.py:106 `APIRouter(prefix="/ui")`).
        // Hitting `/` returns the framework JSON identity payload — which
        // the webview would render as plain text — so always target /ui/.
        let ui_url = format!("http://127.0.0.1:{port}/ui/#nexe_api_key={encoded_key}");

        // macOS/Linux: WKWebView/WebKitGTK honour `navigate()` from any thread.
        #[cfg(not(windows))]
        {
            match ui_url.parse() {
                Ok(url) => {
                    let nav_window = w.clone();
                    if let Err(e) = w.run_on_main_thread(move || match nav_window.navigate(url) {
                        Ok(()) => tracing::info!(port, "splash: navigated webview to sidecar UI"),
                        Err(e) => {
                            tracing::error!(error = %e, port, "splash: webview navigate failed")
                        }
                    }) {
                        tracing::error!(error = %e, port, "splash: run_on_main_thread failed");
                    }
                }
                Err(_) => tracing::warn!(port, "splash: failed to parse sidecar UI URL"),
            }
        }

        // Windows/WebView2 (B038): `navigate()` and `eval()` invoked from Rust do
        // NOT change the page here — off the UI thread they are silent no-ops, and
        // even marshalled onto the main thread WebView2 returns Ok(()) without
        // navigating (verified empirically: log says Ok, window stays on splash).
        // The one path that works on Windows is the native JS navigation the
        // onboarding wizard already uses (step5-apikey.js: `window.location.
        // replace(...)`). We hand the URL to the splash JS via an event and let
        // main.js perform the navigation. The api_key in the URL fragment reaches
        // the splash-origin JS context — the same trusted surface the wizard
        // already relies on (local code, no plugin frames loaded yet, no user
        // input). This is Windows-only; macOS keeps the navigate() path above so
        // its key never transits the JS layer.
        #[cfg(windows)]
        {
            use tauri::Emitter;
            match w.emit("navigate-to-ui", &ui_url) {
                Ok(()) => tracing::info!(port, "splash: emitted navigate-to-ui event to splash JS"),
                Err(e) => tracing::error!(error = %e, port, "splash: emit navigate-to-ui failed"),
            }
        }
    }
}

/// Opens a file or folder with the operating system's associated application.
///
/// macOS: `open <path>` — for `.log` files, macOS launches Console.app by
/// default (integrated auto-tail), matching the original Python tray
/// behaviour (`installer/tray.py:540`). For folders, Finder is used.
fn open_in_system(path: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    let cmd = "open";
    #[cfg(target_os = "linux")]
    let cmd = "xdg-open";
    #[cfg(target_os = "windows")]
    let cmd = "explorer";
    std::process::Command::new(cmd)
        .arg(path)
        .spawn()
        .map(|_| ())
}

/// Values `restart_sidecar` resolves once from Tauri state before it
/// starts killing/spawning. Holds live `State` handles for the two stores it
/// mutates (`SidecarChild` under a Mutex, `SidecarPort` as an Atomic) plus
/// cloned copies of everything else it needs. The lifetime ties the struct to
/// the `AppHandle` it was looked up from.
struct RestartCtx<'a> {
    child_state: tauri::State<'a, SidecarChild>,
    port_state: tauri::State<'a, SidecarPort>,
    sidecar_path: std::path::PathBuf,
    sidecar_data_dir: Option<std::path::PathBuf>,
    stdout_log_path: Option<std::path::PathBuf>,
    auth_token: String,
    api_key: String,
    http_client: reqwest::Client,
}

/// Resolve every piece of state `restart_sidecar` needs. Returns an error
/// naming the first missing state (improbable in production, possible in tests
/// that build a partial app) so the caller bails before killing the sidecar.
fn lookup_restart_state(app: &tauri::AppHandle) -> Result<RestartCtx<'_>, String> {
    use tauri::Manager;

    let child_state = app
        .try_state::<SidecarChild>()
        .ok_or_else(|| "SidecarChild state missing".to_string())?;
    let port_state = app
        .try_state::<SidecarPort>()
        .ok_or_else(|| "SidecarPort state missing".to_string())?;
    let spawn_ctx = app
        .try_state::<SpawnContext>()
        .ok_or_else(|| "SpawnContext state missing".to_string())?;
    let auth_token = app
        .try_state::<AuthToken>()
        .ok_or_else(|| "AuthToken state missing".to_string())?
        .0
        .clone();
    let api_key = app
        .try_state::<ApiKey>()
        .ok_or_else(|| "ApiKey state missing".to_string())?
        .0
        .clone();
    let http_client = app
        .try_state::<HttpClient>()
        .ok_or_else(|| "HttpClient state missing".to_string())?
        .0
        .clone();

    Ok(RestartCtx {
        child_state,
        port_state,
        sidecar_path: spawn_ctx.sidecar_path.clone(),
        sidecar_data_dir: spawn_ctx.sidecar_data_dir.clone(),
        stdout_log_path: spawn_ctx.stdout_log_path.clone(),
        auth_token,
        api_key,
        http_client,
    })
}

/// Spawn a fresh sidecar on `new_port` and register it in state. The state
/// update order is load-bearing: `SidecarChild` (Mutex) is written BEFORE
/// `SidecarPort` (Atomic). The reverse order would open a window where
/// `get_sidecar_port` returns the new port while `kill_sidecar_child` still
/// sees the old child. Returns the new PID for logging.
async fn spawn_and_register_child(ctx: &RestartCtx<'_>, new_port: u16) -> Result<u32, String> {
    let sidecar_path = ctx.sidecar_path.clone();
    let sidecar_data_dir = ctx.sidecar_data_dir.clone();
    let stdout_log_path = ctx.stdout_log_path.clone();
    let auth_token_for_spawn = ctx.auth_token.clone();
    let api_key_for_spawn = ctx.api_key.clone();
    let spawn_result = tauri::async_runtime::spawn_blocking(move || {
        spawn_sidecar_process(
            &sidecar_path,
            &auth_token_for_spawn,
            new_port,
            sidecar_data_dir.as_deref(),
            &api_key_for_spawn,
            stdout_log_path.as_deref(),
        )
    })
    .await
    .map_err(|e| format!("restart_sidecar spawn task join: {e}"))?;
    let child = spawn_result.map_err(|e| format!("restart_sidecar spawn: {e}"))?;
    let new_pid = child.id();

    // Child first (under Mutex), then port (Atomic) — see the doc comment.
    {
        let mut guard = ctx
            .child_state
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = Some(child);
    }
    ctx.port_state.set(new_port);
    Ok(new_pid)
}

/// Poll `/admin/system/health` (500ms interval) up to `deadline_secs` until it
/// returns 2xx. Returns `true` when the sidecar is healthy, `false` on timeout.
/// The supervisor passes a generous boot budget so a slow-booting respawn is not
/// tight-loop-killed; the manual restart command passes the shorter 30s.
async fn wait_for_sidecar_health(
    http_client: &reqwest::Client,
    health_url: &str,
    bearer: &str,
    deadline_secs: u64,
) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(deadline_secs);
    while std::time::Instant::now() < deadline {
        // Bail promptly if the app started shutting down — never keep a doomed
        // (up to 60s) respawn health-wait alive during a quit/uninstall.
        if crate::lifecycle::is_shutting_down() {
            return false;
        }
        match http_client
            .get(health_url)
            .header("Authorization", bearer)
            .timeout(std::time::Duration::from_millis(500))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => return true,
            _ => {
                // Pattern aligned with `poll_sidecar_health` for consistency;
                // tokio is not a direct dependency. The blocking pool overhead
                // is acceptable (60 iter max, 500ms each = bounded).
                tauri::async_runtime::spawn_blocking(|| {
                    std::thread::sleep(std::time::Duration::from_millis(500));
                })
                .await
                .ok();
            }
        }
    }
    false
}

/// Kill the running sidecar, spawn a fresh one on a new ephemeral
/// port, wait for it to become healthy, and emit `sidecar-restarted` with the
/// new port. Returns the new port number to the caller (the onboarding
/// wizard's step 5) so it can navigate the webview accordingly.
///
/// Concurrency: protected by `RESTART_IN_PROGRESS` — a second invocation while
/// one is already in flight returns `Err("RESTART_IN_PROGRESS")` immediately.
///
/// Sequence (helpers extracted 2026-05-30):
///   1. Acquire restart guard (atomic swap).
///   2. Look up state ([`lookup_restart_state`]).
///   3. Graceful POST /admin/system/shutdown, then kill via `kill_sidecar_child` (SIGKILL backstop).
///   4. Reserve a fresh ephemeral port.
///   5. Spawn + register the new sidecar ([`spawn_and_register_child`]).
///   6. Poll `/admin/system/health` up to 30s ([`wait_for_sidecar_health`]);
///      revert `SidecarPort` to `old_port` on timeout.
///   7. Emit `sidecar-restarted` Tauri event ONLY after the health probe passes.
#[tauri::command]
async fn restart_sidecar(app: tauri::AppHandle) -> Result<u16, String> {
    use tauri::Emitter;

    if !restart_try_acquire() {
        tracing::warn!("restart_sidecar invoked while a restart is already in progress");
        return Err("RESTART_IN_PROGRESS".to_string());
    }
    let _guard = RestartGuard;

    let ctx = lookup_restart_state(&app)?;
    let old_port = ctx.port_state.get();
    tracing::info!(old_port, "restart_sidecar: killing current sidecar");

    // B168: graceful POST /admin/system/shutdown BEFORE the hard kill, matching
    // graceful_quit and kill_sidecar_child's documented contract. ctx holds the
    // api_key + http_client resolved before the kill; the old sidecar shares this
    // api_key. Best-effort — kill_sidecar_child still SIGKILLs as the backstop.
    crate::lifecycle::post_sidecar_shutdown(old_port, &ctx.api_key, &ctx.http_client).await;
    // The returned PID (if any) is purely informational here.
    let _killed_pid = crate::lifecycle::kill_sidecar_child(&ctx.child_state.0);

    let new_port =
        reserve_ephemeral_port().map_err(|e| format!("restart_sidecar reserve port: {e}"))?;
    verify_port_free(new_port)
        .map_err(|e| format!("restart_sidecar verify port {new_port}: {e}"))?;
    tracing::info!(new_port, "restart_sidecar: spawning fresh sidecar");

    let new_pid = spawn_and_register_child(&ctx, new_port).await?;
    tracing::info!(new_pid, new_port, "restart_sidecar: new sidecar registered");

    let health_url = format!("http://127.0.0.1:{new_port}/admin/system/health");
    let bearer = format!("Bearer {}", ctx.auth_token);
    if !wait_for_sidecar_health(&ctx.http_client, &health_url, &bearer, 30).await {
        // Revert port to the old value. The new sidecar is dead and
        // the old one was already killed, so neither is serving. Reverting
        // gives the frontend a clear "connection refused" on old_port rather
        // than a silent hang on new_port that has no listener.
        ctx.port_state.set(old_port);
        tracing::warn!(
            old_port,
            new_port,
            "restart_sidecar: new sidecar unhealthy — reverting port_state to old_port"
        );
        return Err("restart_sidecar: new sidecar did not become healthy within 30s".to_string());
    }
    tracing::info!(new_port, "restart_sidecar: new sidecar healthy");

    // Emit event AFTER health passes so the frontend never targets a port that
    // is still booting.
    let _ = app.emit("sidecar-restarted", new_port);

    Ok(new_port)
}

/// Same-port respawn used by the runtime supervisor (WSH-001). Unlike the manual
/// `restart_sidecar` command it (a) reuses the CURRENT port instead of reserving
/// a new one — so the plugin UI reconnects on its own `/status` poll with no
/// re-navigation (impossible cleanly on WebView2), and (b) does not emit
/// `sidecar-restarted` (the http-origin UI never receives Tauri events anyway).
/// `kill_sidecar_child` is idempotent: a crashed child is already dead (fast
/// path), a hung one is SIGKILLed and its port freed. The auth token is reused
/// so the UI's session stays valid across the respawn.
async fn respawn_same_port(app: tauri::AppHandle) -> Result<(), String> {
    // R4 / external review ALT 1: never respawn while the app is closing/uninstalling.
    if crate::lifecycle::is_shutting_down() {
        return Err("shutdown in progress — skipping respawn".to_string());
    }
    let ctx = lookup_restart_state(&app)?;
    let port = ctx.port_state.get();
    // Reap the dead child, or kill a hung one — frees the port either way.
    let _ = crate::lifecycle::kill_sidecar_child(&ctx.child_state.0);
    verify_port_free(port).map_err(|e| format!("respawn_same_port verify {port}: {e}"))?;
    // R4 / external review ALT 1: final shutdown check right before the spawn — the kill
    // above can take up to 1.5s, and a quit may have begun in that window; never
    // spawn a fresh sidecar the shutdown path is about to (or already did) kill.
    if crate::lifecycle::is_shutting_down() {
        return Err("shutdown began during respawn — aborting before spawn".to_string());
    }
    let new_pid = spawn_and_register_child(&ctx, port).await?;
    // Review HIGH: a shutdown can begin DURING the spawn. The shutdown path
    // (quit/uninstall) kills the sidecar exactly once by reading the SidecarChild
    // slot — which was `None` while we were spawning — so its kill misses this
    // fresh child. The child runs in its own process group, so `app.exit(0)` will
    // NOT reap it → it would orphan (corrupting an uninstall wipe / holding the
    // port). Close the window: if a shutdown started, kill the child we just
    // registered before it can leak.
    if crate::lifecycle::is_shutting_down() {
        let _ = crate::lifecycle::kill_sidecar_child(&ctx.child_state.0);
        return Err("shutdown began during respawn — killed the fresh sidecar".to_string());
    }
    let health_url = format!("http://127.0.0.1:{port}/admin/system/health");
    let bearer = format!("Bearer {}", ctx.auth_token);
    if !wait_for_sidecar_health(
        &ctx.http_client,
        &health_url,
        &bearer,
        crate::sidecar::RESPAWN_HEALTH_TIMEOUT_SECS,
    )
    .await
    {
        // If a shutdown began during the wait (health-wait bailed early), the
        // fresh child is registered and alive — kill it now so it cannot linger
        // touching storage during an uninstall wipe (Round-3 LOW: full_uninstall
        // races a booting sidecar).
        if crate::lifecycle::is_shutting_down() {
            let _ = crate::lifecycle::kill_sidecar_child(&ctx.child_state.0);
        }
        return Err(format!(
            "respawn_same_port: sidecar on {port} not healthy within {}s",
            crate::sidecar::RESPAWN_HEALTH_TIMEOUT_SECS
        ));
    }
    tracing::info!(
        new_pid,
        port,
        "supervisor: sidecar respawned on same port, healthy"
    );
    Ok(())
}

/// One-shot liveness probe used to CONFIRM the sidecar is genuinely down right
/// before the supervisor kills+respawns it. Between deciding "dead" and acting,
/// the supervisor sleeps a backoff without holding the restart guard, so the
/// sidecar may have recovered (self-heal, or a manual/onboarding restart that
/// finished). Returns true if it now looks HEALTHY (process alive AND health
/// endpoint 2xx) — in which case the supervisor must NOT kill it.
async fn supervisor_sidecar_healthy(app: &tauri::AppHandle) -> bool {
    let (port, client, token) = {
        let ctx = match lookup_restart_state(app) {
            Ok(c) => c,
            Err(_) => return false,
        };
        if crate::sidecar::child_has_exited(&ctx.child_state.0) {
            return false; // process gone → not healthy
        }
        (
            ctx.port_state.get(),
            ctx.http_client.clone(),
            ctx.auth_token.clone(),
        )
    };
    let url = format!("http://127.0.0.1:{port}/admin/system/health");
    // A single loopback probe can transiently fail (the sidecar momentarily busy
    // on its event loop); a false negative here would kill a HEALTHY sidecar that
    // just recovered during the backoff. Require 3 consecutive misses before
    // concluding "down" — healthy if ANY probe succeeds (Review MEDIUM).
    for probe in 0..3u8 {
        if probe > 0 {
            let _ = tauri::async_runtime::spawn_blocking(|| {
                std::thread::sleep(std::time::Duration::from_millis(200))
            })
            .await;
        }
        if matches!(
            client
                .get(&url)
                .header("Authorization", format!("Bearer {token}"))
                .timeout(std::time::Duration::from_millis(500))
                .send()
                .await,
            Ok(resp) if resp.status().is_success()
        ) {
            return true;
        }
    }
    false
}

/// Runtime supervisor (WSH-001): watches the sidecar and respawns it on the same
/// port if it dies (process exit) or hangs (`HB_FAIL_THRESHOLD` consecutive HTTP
/// health failures), with a backoff that NEVER gives up permanently. Idle while
/// the app is shutting down or a manual restart is in flight. Spawned once at
/// `setup_services`; runs until `app.exit(0)`.
async fn sidecar_supervisor(app: tauri::AppHandle) {
    let mut hb_fails: u32 = 0;
    let mut attempt: u32 = 0;
    // Has the app EVER been healthy in its lifetime? LATCH — set once, never
    // reset. Hang-detection (hb_fails) must not fire before this, so the very
    // first boot is deferred to poll_sidecar_health's 120s budget and a booting
    // sidecar is never murdered. But it must NOT go back to false after a
    // respawn: a hung/failed respawn generation still has to be detected and
    // retried, or the supervisor goes permanently blind (Round-2 HIGH). A
    // still-booting *respawn* is instead protected by respawn_same_port's
    // generous health-wait (RESPAWN_HEALTH_TIMEOUT_SECS), not by this latch. A
    // definite process crash always restarts regardless of the latch.
    let mut ever_healthy = false;

    loop {
        // Interval sleep off the async reactor (tokio is not a direct dep).
        let interval = crate::sidecar::SUPERVISOR_POLL_INTERVAL;
        let _ = tauri::async_runtime::spawn_blocking(move || std::thread::sleep(interval)).await;

        if crate::lifecycle::is_shutting_down() {
            tracing::info!("supervisor: shutdown detected — stopping");
            return;
        }
        if crate::sidecar::RESTART_IN_PROGRESS.load(Ordering::Acquire) {
            // A manual/onboarding restart owns the sidecar — reset our failure
            // counter + backoff ladder and let it take over (ever_healthy is a
            // lifetime latch, never reset).
            hb_fails = 0;
            attempt = 0;
            continue;
        }

        // Detection: read the process-liveness bit + owned health-probe inputs
        // WITHOUT holding the Tauri State across the HTTP await below.
        let (proc_dead, port, client, token) = {
            let ctx = match lookup_restart_state(&app) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(error = %e, "supervisor: state lookup failed");
                    continue;
                }
            };
            // R5/R6: never take(), poison-recovered lock — see child_has_exited.
            let proc_dead = crate::sidecar::child_has_exited(&ctx.child_state.0);
            (
                proc_dead,
                ctx.port_state.get(),
                ctx.http_client.clone(),
                ctx.auth_token.clone(),
            )
        };

        // HTTP liveness (external review BLOCKER 1): catches a hung-but-alive sidecar that
        // `try_wait` alone would miss.
        let http_ok = {
            let url = format!("http://127.0.0.1:{port}/admin/system/health");
            matches!(
                client
                    .get(&url)
                    .header("Authorization", format!("Bearer {token}"))
                    .timeout(std::time::Duration::from_millis(1500))
                    .send()
                    .await,
                Ok(resp) if resp.status().is_success()
            )
        };
        if http_ok {
            ever_healthy = true;
            hb_fails = 0;
            attempt = 0; // healthy → reset the backoff ladder
        } else {
            hb_fails = hb_fails.saturating_add(1);
        }
        // A crash (definite process exit) always counts. A hang (HTTP failing on a
        // still-running process) counts ONLY once the app has EVER been healthy —
        // defers the first boot to poll_sidecar_health; stays armed thereafter.
        let dead = crate::sidecar::sidecar_dead(proc_dead, ever_healthy, hb_fails);

        if !crate::sidecar::should_restart(
            dead,
            crate::lifecycle::is_shutting_down(),
            crate::sidecar::RESTART_IN_PROGRESS.load(Ordering::Acquire),
        ) {
            continue;
        }

        // Backoff BEFORE taking the exclusive restart guard, so a manual/onboarding
        // restart is never blocked while we wait (Review MEDIUM). Never gives up —
        // supervisor_backoff saturates at the ceiling.
        let delay = crate::sidecar::supervisor_backoff(attempt);
        attempt = attempt.saturating_add(1);
        if !delay.is_zero() {
            let _ = tauri::async_runtime::spawn_blocking(move || std::thread::sleep(delay)).await;
        }
        // Re-evaluate after the (possibly long) backoff sleep: shutdown may have
        // begun, or a manual restart may have taken over.
        if crate::lifecycle::is_shutting_down() {
            return;
        }
        if crate::sidecar::RESTART_IN_PROGRESS.load(Ordering::Acquire) {
            hb_fails = 0;
            attempt = 0;
            continue;
        }
        // Serialize against a concurrent manual restart (R3).
        if !restart_try_acquire() {
            continue;
        }
        let _guard = RestartGuard;

        // The sidecar may have recovered DURING the backoff (self-heal, or a
        // manual restart that finished). Confirm it is still down before we
        // kill+respawn — never kill a healthy sidecar (self-review).
        if supervisor_sidecar_healthy(&app).await {
            ever_healthy = true;
            hb_fails = 0;
            attempt = 0;
            continue;
        }

        match respawn_same_port(app.clone()).await {
            Ok(()) => {
                // Respawn confirmed healthy (wait_for_sidecar_health passed).
                ever_healthy = true;
                hb_fails = 0;
                attempt = 0;
            }
            Err(e) => {
                // The respawn spent its full generous health budget without
                // becoming healthy → genuinely failed. Keep detection armed
                // (ever_healthy is a lifetime latch) so the next cycle re-detects
                // via hb_fails/proc_dead and retries with a longer backoff —
                // NEVER go blind here (Round-2 HIGH). hb_fails resets so the
                // re-detection restarts cleanly.
                tracing::warn!(error = %e, attempt, "supervisor: respawn failed — will retry");
                hb_fails = 0;
            }
        }
        // `_guard` drops here → RESTART_IN_PROGRESS released.
    }
}

fn setup_services(app: &mut tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    // TOCTOU guard — MUST run before the sidecar extraction below writes
    // `.extracted` for this very session: snapshot whether a PREVIOUS session
    // left a partial install behind. `check_partial_install` reads this
    // snapshot instead of the live filesystem (see onboarding_cmd.rs).
    app.manage(crate::onboarding_cmd::PartialInstallAtBoot(
        std::sync::atomic::AtomicBool::new(crate::onboarding_cmd::detect_partial_install(
            app.handle(),
        )),
    ));

    app.manage(AuthToken::generate());
    app.manage(ApiKey::generate());
    tracing::info!("auth token + api key generated (uuid v4, 128 bits entropy each)");

    let sidecar_path = resolve_sidecar_path(app.handle()).map_err(|e| {
        tracing::error!(error = %e, "could not resolve the sidecar path");
        e
    })?;
    let auth_token = app.state::<AuthToken>().0.clone();
    let api_key = app.state::<ApiKey>().0.clone();

    let sidecar_port = reserve_ephemeral_port().map_err(|e| {
        tracing::error!(error = %e, "could not reserve ephemeral port");
        e
    })?;
    verify_port_free(sidecar_port).map_err(|e| {
        tracing::error!(error = %e, port = sidecar_port, "port taken before spawn");
        e
    })?;
    tracing::info!(sidecar = %sidecar_path.display(), port = sidecar_port, "spawning sidecar");

    // Dev: target/sidecar/ is used directly (launcher finds venv via $(dirname $0)).
    // Release: extract sidecar-bundle.tar.gz to app_data_dir/sidecar/ and pass the
    // path via NEXE_SIDECAR_DIR so the launcher finds venv/ and app/ there.
    #[cfg(debug_assertions)]
    let sidecar_data_dir: Option<std::path::PathBuf> = None;
    #[cfg(not(debug_assertions))]
    let sidecar_data_dir: Option<std::path::PathBuf> = Some(
        crate::sidecar_extract::ensure_sidecar_extracted(app.handle()).map_err(|e| {
            tracing::error!(error = %e, "sidecar bundle extraction failed");
            e
        })?,
    );

    // Step 0: if we have sidecar_data_dir (release), capture stdout/stderr to
    // <data_dir>/logs/sidecar-stdout.log so the tray can open it with
    // Console.app (macOS associates `.log` by default) and see pre-logger crashes.
    let sidecar_stdout_log: Option<std::path::PathBuf> = sidecar_data_dir
        .as_ref()
        .map(|d| d.join("logs").join("sidecar-stdout.log"));

    let child = spawn_sidecar_process(
        &sidecar_path,
        &auth_token,
        sidecar_port,
        sidecar_data_dir.as_deref(),
        &api_key,
        sidecar_stdout_log.as_deref(),
    )?;
    let pid = child.id();
    app.manage(SidecarChild(Mutex::new(Some(child))));
    app.manage(SidecarPort::new(sidecar_port));
    // Persist the spawn context so `restart_sidecar` can re-invoke
    // `spawn_sidecar_process` with the same paths the initial setup used.
    app.manage(SpawnContext {
        sidecar_path: sidecar_path.clone(),
        sidecar_data_dir: sidecar_data_dir.clone(),
        stdout_log_path: sidecar_stdout_log.clone(),
    });
    if let Some(path) = sidecar_stdout_log {
        app.manage(SidecarLogPath(path));
    }
    tracing::info!(pid, port = sidecar_port, "sidecar spawned");

    // SSRF mitigation: disable HTTP redirects on the shared reqwest client.
    // The webview proxies requests through this client to the local sidecar; an
    // attacker that controls a sidecar response (e.g. via a malicious plugin)
    // could otherwise redirect the bearer token to an arbitrary host. Local-only
    // traffic has no legitimate need for cross-host redirects.
    let http_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| {
            tracing::error!(error = %e, "reqwest::Client::builder failed");
            format!("reqwest builder: {e}")
        })?;
    let health_client = http_client.clone();
    app.manage(HttpClient(http_client));
    tracing::info!("shared reqwest::Client registered (timeout 30s)");

    let app_handle = app.handle().clone();
    tauri::async_runtime::spawn(poll_sidecar_health(
        app_handle,
        sidecar_port,
        auth_token,
        api_key,
        health_client,
    ));

    // WSH-001: runtime supervisor — respawns the sidecar on the same port if it
    // dies/hangs so the plugin UI reconnects on its own `/status` poll.
    tauri::async_runtime::spawn(sidecar_supervisor(app.handle().clone()));

    Ok(())
}

/// Builds the tray menu and registers event handlers.
/// Extracted from run() (2026-05-08) to reduce CCN of the root function.
fn build_tray_menu(app: &mut tauri::App) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show", "Show nexe-app", true, None::<&str>)?;
    let hide = MenuItem::with_id(app, "hide", "Hide nexe-app", true, None::<&str>)?;
    let sep_logs = PredefinedMenuItem::separator(app)?;
    // Step 0 (tray logs viewer) — restores the behaviour of the original Python tray
    // (installer/tray.py:540 _open_logs) that opened server.log with Console.app.
    let open_log = MenuItem::with_id(
        app,
        "open_sidecar_log",
        "Open sidecar log",
        true,
        None::<&str>,
    )?;
    let open_logs_dir = MenuItem::with_id(
        app,
        "open_logs_folder",
        "Open logs folder",
        true,
        None::<&str>,
    )?;
    let separator = PredefinedMenuItem::separator(app)?;
    let uninstall = MenuItem::with_id(app, "uninstall", "Uninstall…", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[
            &show,
            &hide,
            &sep_logs,
            &open_log,
            &open_logs_dir,
            &separator,
            &uninstall,
            &quit,
        ],
    )?;

    let tray_icon = tauri::include_image!("icons/tray.png");
    TrayIconBuilder::with_id("main")
        .menu(&menu)
        .tooltip("nexe-app")
        .icon(tray_icon)
        .icon_as_template(false)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "show" => {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.set_focus();
                }
            }
            "hide" => {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.hide();
                }
            }
            "open_sidecar_log" => {
                // macOS: `open <file.log>` → Console.app per defecte (auto-tail).
                // Linux/Windows: file manager o associated viewer.
                if let Some(state) = app.try_state::<SidecarLogPath>() {
                    let path = state.0.clone();
                    if !path.exists() {
                        // Fall back to the directory if the log has not been
                        // generated yet (sidecar never started or crashed at
                        // the first instant without writing).
                        if let Some(parent) = path.parent() {
                            let _ = open_in_system(parent);
                        }
                    } else {
                        let _ = open_in_system(&path);
                    }
                } else {
                    tracing::warn!(
                        "open_sidecar_log: SidecarLogPath state not registered (dev mode?)"
                    );
                }
            }
            "open_logs_folder" => {
                if let Some(state) = app.try_state::<SidecarLogPath>() {
                    if let Some(parent) = state.0.parent() {
                        let _ = open_in_system(parent);
                    }
                } else {
                    // B170: symmetric to open_sidecar_log — in dev mode SidecarLogPath
                    // is not registered, so both tray items are no-ops; warn instead of
                    // failing silently.
                    tracing::warn!(
                        "open_logs_folder: SidecarLogPath state not registered (dev mode?)"
                    );
                }
            }
            "uninstall" => {
                // Finding B: the uninstall is a SELECTIVE modal (checkboxes for
                // models / conversations / library / ollama) rendered in a
                // DEDICATED Tauri window (label "uninstall", page uninstall.html)
                // so the user can pick exactly what to wipe for a clean reinstall.
                //
                // Why a dedicated window and NOT an event to the main webview:
                // after onboarding the main webview navigates to the sidecar HTTP
                // origin (main.js `window.location.replace(...)`), where our JS no
                // longer runs — an emitted `open-uninstall-dialog` would be dead
                // exactly in the normal post-onboarding case. The window always
                // works. The actual removal runs in `uninstall_with_options`,
                // which has its OWN native confirmation gate (WSA-002) and
                // replicates the shutdown concurrency contract (WSH-001/B058/
                // MC-057). `full_uninstall`/`reset_paths` are kept for compat +
                // tests.
                tracing::info!("uninstall from tray — opening dedicated window");
                if let Some(win) = app.get_webview_window("uninstall") {
                    // Already open — surface it instead of stacking a second one.
                    let _ = win.show();
                    let _ = win.set_focus();
                } else {
                    // WebView2 (Windows): tots els webviews del mateix procés han
                    // d'usar els MATEIXOS additionalBrowserArguments. La finestra
                    // "main" posa --host-resolver-rules a tauri.conf.json; una 2a
                    // finestra creada sense ells rebria el default de wry
                    // (--disable-features només) → l'entorn compartit no coincideix i
                    // CreateCoreWebView2Controller falla amb ERROR_INVALID_STATE
                    // (0x8007139F): uninstall.html no carrega i uninstall_with_options
                    // no s'arriba a invocar. Heretem els args de la "main" perquè
                    // coincideixin (no-op a macOS/Linux, on aquest flag no s'aplica).
                    let mut builder = tauri::WebviewWindowBuilder::new(
                        app,
                        "uninstall",
                        tauri::WebviewUrl::App("uninstall.html".into()),
                    )
                    .title("Uninstall nexe-app")
                    .inner_size(480.0, 460.0)
                    .resizable(false)
                    .center();
                    if let Some(args) = app
                        .config()
                        .app
                        .windows
                        .iter()
                        .find(|w| w.label == "main")
                        .and_then(|w| w.additional_browser_args.clone())
                    {
                        builder = builder.additional_browser_args(&args);
                    }
                    match builder.build() {
                        Ok(_) => tracing::info!("uninstall window created"),
                        Err(e) => tracing::error!(error = %e, "failed to open uninstall window"),
                    }
                }
            }
            "quit" => {
                // Tray Quit → centralized graceful_quit.
                // IMPORTANT: show main window BEFORE the dialog.
                // Without a visible window, the MessageDialog has no parent and
                // does not render in Windows WebView2 (silent fail runtime bug 2026-04-19).
                tracing::info!("quit from tray");
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.set_focus();
                }
                graceful_quit(app);
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                let app = tray.app_handle();
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.set_focus();
                }
            }
        })
        .build(app)?;
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Linux GPU-less rendering fix. WebKitGTK 2.42+ (e.g. 2.52 on Ubuntu 24.04)
    // enables DMABUF/GPU rendering by default. On machines without a working
    // GPU/EGL stack (VMs, headless servers, some ARM setups) that path fails to
    // initialize ("failed to create dri2 screen") and the process aborts before
    // any window appears. Force the non-DMABUF (software-friendly) path so the
    // app renders via CPU on GPU-less machines. Set only when the user hasn't
    // chosen a value, so it stays overridable. Must run before any GTK/WebKit
    // init (i.e. before `tauri::Builder`). No-op on macOS/Windows.
    #[cfg(target_os = "linux")]
    if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() {
        std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
    }

    // ADR-0017 (2026-04-22) — single logger pipeline. `logging::init()` configures
    // `tracing-subscriber` with stdout + file rolling daily layers
    // (`data_local_dir()/com.nexe.app/logs/`). No direct `log::set_logger` here:
    // `tracing-subscriber` with the default `tracing-log` feature installs
    // a global `LogTracer` automatically, redirecting `log::*` (internal tauri,
    // third-party deps) → `tracing::*`. Replaces `tauri-plugin-log`, which
    // caused a `SetLoggerError` conflict in Phase 0 (2026-04-22).
    crate::logging::init();

    tauri::Builder::default()
        // single-instance FIRST (prerequisite for other plugins that may be sensitive
        // to multi-launch). When the user opens a second instance, show the existing window.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            tracing::info!("second instance launch — focusing existing window");
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
                let _ = window.unminimize();
                let _ = window.set_focus();
            }
        }))
        // dialog (ask/confirm/message/open/save).
        .plugin(tauri_plugin_dialog::init())
        // ADR-0017 (2026-04-22): `tauri-plugin-log` removed. Unified logging
        // via `tracing-subscriber` + `tracing-appender` (rolling daily file at
        // `data_local_dir()/com.nexe.app/logs/`). Initialized by
        // `logging::init()` above, before the Builder.
        //
        // (2026-04-21): `tauri_plugin_store` +
        // `tauri_plugin_notification` already removed in that sprint.
        //
        // (2026-05-18): tauri-plugin-deep-link removed. The plugin
        // exposed an unsanitised `nexe://` URL handler that a hostile web page
        // could weaponise (the OAuth callback hook was never wired). If we
        // need OS-level deep links again we will reintroduce them with a
        // sanitised handler from scratch, not the default plugin.
        // (2026-04-21): async handler via bounded threadpool
        // (8 workers) with pre-queue validation + rate-limit + bounded
        // queue. Reject fraudulent requests before enqueuing → DoS protection.
        .register_asynchronous_uri_scheme_protocol(
            "plugin",
            |ctx: UriSchemeContext<'_, _>, request, responder| {
                let app = ctx.app_handle().clone();

                // C06: PRE-QUEUE validation + rate-limit + bounded.
                // Without this, rate-limit was applied INSIDE the worker (too late);
                // a pre-filter flood could fill the threadpool mpsc queue
                // without bound → OOM before any 429 is processed.
                let method = request.method().as_str().to_string();
                let uri_str = request.uri().to_string();

                if let Err(status) = validate_request(&method, request.uri()) {
                    responder.respond(err_response(status, b"bad request"));
                    return;
                }
                let plugin_id = match extract_plugin_id_from_uri(&uri_str) {
                    Some(id) => id,
                    None => {
                        responder.respond(err_response(400, b"invalid plugin path"));
                        return;
                    }
                };
                // WSC-003: per-plugin bucket (fairness) + shared GLOBAL bucket.
                // plugin_id comes from the attacker-controlled URI, so the
                // per-id bucket alone is evadable by minting fresh ids; the
                // global bucket bounds total throughput regardless.
                if !plugin_rate_limits_ok(&plugin_id) {
                    responder.respond(err_response(429, b"too many requests"));
                    return;
                }

                // CAS refactor (2026-04-22): the CAS logic
                // lives in `try_acquire_pending_slot()` so the B3 test can call
                // the SAME helper as the production code (no replication in test).
                // If it returns `None`, queue full → 503. If it returns `Some(guard)`,
                // the guard lives until Drop at the end of the worker closure (RAII decrement
                // including panic, unwind mode; in abort release mode the whole process
                // crashes and the counter is irrelevant).
                let guard = match try_acquire_pending_slot() {
                    Some(g) => g,
                    None => {
                        tracing::warn!(
                            pending = PENDING_COUNT.load(Ordering::Acquire),
                            max = MAX_QUEUED,
                            "plugin:// queue full — rejecting 503"
                        );
                        responder.respond(err_response(503, b"service unavailable (queue full)"));
                        return;
                    }
                };

                handler_pool().execute(move || {
                    // The guard lives throughout the worker's work. On Drop
                    // (natural end or panic unwind) decrements PENDING_COUNT.
                    let _guard = guard;

                    let response = plugin_protocol_handler(&app, request);
                    responder.respond(response);
                });
            },
        )
        .setup(|app| {
            setup_services(app)?;
            build_tray_menu(app)?;
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                // C1 (HIGH): the "Quit?" confirmation belongs ONLY to the main
                // window. This handler is global, so without the label branch
                // Cancel/X on the dedicated "uninstall" window (or any future
                // auxiliary window) would prevent_close + graceful_quit the WHOLE
                // app — closing the dialog would try to quit nexe-app. For any
                // non-main label we leave `api` untouched so Tauri closes just
                // that window and the app keeps running.
                if window.label() != "main" {
                    return;
                }
                // Unification 2026-04-19: X, Alt+F4, tray Quit all show the same
                // "are you sure?" dialog. X no longer does silent hide. To hide
                // without closing, use the "Hide" option in the tray menu.
                api.prevent_close();
                graceful_quit(window.app_handle());
            }
        })
        // Registered commands.
        // Security (2026-04-21): `fetch_from_sidecar` injects the Bearer token
        // on the Rust side (never exposed to the main webview). `get_auth_token` removed
        // security audit: exposed the raw token via XSS.
        .invoke_handler(tauri::generate_handler![
            greet,
            quit_app,
            fetch_from_sidecar,
            get_sidecar_port,
            // onboarding commands — short names match isolation.js allowlist + frontend invoke()
            get_hardware,
            fetch_catalog,
            check_first_run,
            mark_onboarding_complete,
            // partial install detection + reset (Step 1 banner)
            check_partial_install,
            reset_installation,
            // open external URLs in system browser (target="_blank" workaround)
            open_external_url,
            // restart sidecar to pick up post-wizard onboarding state.
            restart_sidecar,
            // Finding B: selective uninstall (models/conversations/library/ollama)
            // driven by the frontend modal; has its OWN native WSA-002 gate.
            uninstall_with_options
        ])
        // unwrap_or_else — clear message + exit(1) without panic
        .build(tauri::generate_context!())
        .unwrap_or_else(|e| {
            eprintln!("[nexe-app] fatal: failed to build app: {e}");
            std::process::exit(1);
        })
        .run(|_app_handle, event| {
            // ExitRequested → centralized graceful_quit.
            // CRITICAL: api.prevent_exit() prevents Tauri from executing app.exit(0) automatically
            // while we wait for the user in the confirmation dialog. Only graceful_quit
            // (via dialog callback) decides if we really exit.
            //
            // EXIT_CONFIRMED flag: when graceful_quit callback has confirmed exit and
            // calls app.exit(0), Tauri fires ExitRequested again. To avoid
            // vicious cycle (dialog inside dialog) we let it pass without prevent_exit.
            if let tauri::RunEvent::ExitRequested { api, .. } = &event {
                if EXIT_CONFIRMED.load(Ordering::Relaxed) {
                    tracing::info!("ExitRequested post-confirm — letting Tauri exit");
                    return;
                }
                tracing::info!("ExitRequested — prevent_exit + graceful_quit");
                api.prevent_exit();
                graceful_quit(_app_handle);
            }

            // macOS dock reopen — restores hidden window on dock click
            #[cfg(target_os = "macos")]
            if let tauri::RunEvent::Reopen {
                has_visible_windows,
                ..
            } = &event
            {
                if !has_visible_windows {
                    if let Some(w) = _app_handle.get_webview_window("main") {
                        let _ = w.show();
                        let _ = w.set_focus();
                    }
                }
            }
        });
}

// unit tests for the resolver (no Tauri runtime needed)
#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::handler_pool;
    use std::fs;
    use std::io::Read;
    use std::path::{Path, PathBuf};

    fn mktemp_root(test_name: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "nexe-app-test-{}-{}",
            test_name,
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        base
    }

    fn mk_plugin(root: &Path, id: &str, file_rel: &str, content: &str) {
        let ui = root.join(id).join("ui");
        fs::create_dir_all(&ui).unwrap();
        let file = ui.join(file_rel);
        if let Some(parent) = file.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&file, content).unwrap();
    }

    // The resolve_sidecar_path_* and kill_sidecar_child tests have
    // been moved to `sidecar.rs` (mod tests) — 2026-05-08.

    // escape_js_string defensive coverage was removed
    // with the revert: the api_key now travels as a URL query param
    // (percent-encoded), not as an inlined JS string literal, so the helper
    // is no longer wired into any production path. If a webview script-
    // injection path returns, re-introduce the helper together with the full
    // test suite.

    #[test]
    fn content_type_html() {
        assert_eq!(content_type_for("index.html"), "text/html; charset=utf-8");
    }

    #[test]
    fn health_poll_timeout_not_less_than_startup_timeout() {
        // B169: the splash health poll must not give up before the sidecar's
        // own startup budget (sidecar_extract deadline = 120s). 90s is the
        // regression floor that keeps the two in sync.
        assert!(HEALTH_POLL_TIMEOUT_SECS >= 90);
    }

    #[test]
    fn content_type_css() {
        assert_eq!(content_type_for("style.css"), "text/css; charset=utf-8");
    }

    #[test]
    fn content_type_js() {
        assert_eq!(
            content_type_for("app.js"),
            "application/javascript; charset=utf-8"
        );
    }

    #[test]
    fn content_type_unknown_fallback() {
        assert_eq!(content_type_for("file.xyz"), "application/octet-stream");
    }

    // new formats
    #[test]
    fn content_type_webp() {
        assert_eq!(content_type_for("logo.webp"), "image/webp");
    }

    #[test]
    fn content_type_woff2() {
        assert_eq!(content_type_for("font.woff2"), "font/woff2");
    }

    #[test]
    fn content_type_wasm() {
        assert_eq!(content_type_for("mod.wasm"), "application/wasm");
    }

    #[test]
    fn content_type_case_insensitive() {
        // MIME sniffing vuln: .HTML must not fall back to octet-stream
        assert_eq!(content_type_for("INDEX.HTML"), "text/html; charset=utf-8");
    }

    // validate_plugin_id
    #[test]
    fn plugin_id_valid_alphanumeric() {
        assert!(validate_plugin_id("rag"));
        assert!(validate_plugin_id("my-plugin"));
        assert!(validate_plugin_id("plugin_123"));
        assert!(validate_plugin_id("a1"));
    }

    #[test]
    fn plugin_id_rejects_path_traversal() {
        assert!(!validate_plugin_id("../etc/passwd"));
        assert!(!validate_plugin_id(".."));
        assert!(!validate_plugin_id("plug/in"));
    }

    #[test]
    fn plugin_id_rejects_special_chars() {
        assert!(!validate_plugin_id("<script>"));
        assert!(!validate_plugin_id("plug in"));
        assert!(!validate_plugin_id("plug.in"));
        assert!(!validate_plugin_id("plug:in"));
    }

    #[test]
    fn plugin_id_rejects_too_short_or_long() {
        assert!(!validate_plugin_id(""));
        assert!(!validate_plugin_id("a")); // 1 char, below minimum
        let long = "a".repeat(65);
        assert!(!validate_plugin_id(&long));
        let max = "a".repeat(64);
        assert!(validate_plugin_id(&max));
    }

    // Windows reserved device names (cross-platform, no cfg)
    #[test]
    fn plugin_id_rejects_windows_reserved_names() {
        // DOS device names — Windows does not allow creating them as directories.
        assert!(!validate_plugin_id("con"));
        assert!(!validate_plugin_id("prn"));
        assert!(!validate_plugin_id("aux"));
        assert!(!validate_plugin_id("nul"));
        // COM1-9
        for n in 1..=9 {
            assert!(
                !validate_plugin_id(&format!("com{n}")),
                "com{n} must be reserved"
            );
            assert!(
                !validate_plugin_id(&format!("lpt{n}")),
                "lpt{n} must be reserved"
            );
        }
    }

    // names that merely CONTAIN reserved names are still valid
    #[test]
    fn plugin_id_accepts_names_containing_reserved() {
        // "con" alone is reserved, but "con-plugin" or "my-con" are valid.
        assert!(validate_plugin_id("con-plugin"));
        assert!(validate_plugin_id("my-con"));
        assert!(validate_plugin_id("prn123"));
        assert!(validate_plugin_id("com10")); // > 9 is not reserved
        assert!(validate_plugin_id("com0")); // 0 is not reserved
        assert!(validate_plugin_id("lpt0"));
    }

    // Windows-specific protections (CI Windows only).
    // In the current CI (macOS+Linux+Windows) these tests only run on Windows.
    #[cfg(windows)]
    #[test]
    fn unc_prefix_consistent_resolution() {
        // Windows UNC prefix `\\?\C:\...` — canonicalize may add the prefix.
        // We want to verify that starts_with comparison still works correctly.
        let root = mktemp_root("unc");
        mk_plugin(&root, "rag", "index.html", "<h1>ok</h1>");
        let res = resolve_plugin_path(&root, "rag", "/index.html");
        assert!(res.is_ok(), "resolve with UNC path failed: {:?}", res);
    }

    // APFS/NTFS case-insensitive protection (validate_plugin_id already covers it
    // by rejecting uppercase, but we add an explicit test for Windows NTFS).
    #[cfg(windows)]
    #[test]
    fn ntfs_case_insensitive_protection() {
        // On Windows NTFS it is case-insensitive by default.
        // validate_plugin_id rejects uppercase, so `plugin://RAG/...` never reaches
        // resolve with "RAG". Cross-platform regression.
        assert!(!validate_plugin_id("RAG"));
        assert!(!validate_plugin_id("Rag"));
        let root = mktemp_root("ntfs_case");
        mk_plugin(&root, "rag", "index.html", "ok");
        // Attempting to open with uppercase id → 400 bad request (validate_plugin_id fail)
        assert_eq!(resolve_plugin_path(&root, "RAG", "/index.html"), Err(400));
    }

    // MAX_PATH (260 chars) Windows must not panic or crash.
    // A path of >260 chars on Windows returns err from canonicalize → 404 or 400.
    #[cfg(windows)]
    #[test]
    fn max_path_windows_does_not_panic() {
        let root = mktemp_root("maxpath");
        // max plugin_id (64) + very long path
        let long_path = format!("/{}.html", "a".repeat(300));
        let res = resolve_plugin_path(&root, "plug", &long_path);
        // Must return Err (404 or 400), never panic
        assert!(res.is_err(), "very long path must return Err, not panic");
    }

    // APFS case-insensitive cross-platform bug protection
    #[test]
    fn plugin_id_uppercase_rejected() {
        assert!(!validate_plugin_id("RAG"));
        assert!(!validate_plugin_id("Rag"));
        assert!(!validate_plugin_id("rAg"));
        assert!(!validate_plugin_id("Plugin-123"));
    }

    #[test]
    fn resolve_empty_plugin_id_rejects_400() {
        let root = mktemp_root("empty");
        assert_eq!(resolve_plugin_path(&root, "", "index.html"), Err(400));
    }

    #[test]
    fn resolve_invalid_plugin_id_rejects_400() {
        let root = mktemp_root("invalid_id");
        // disallowed character
        assert_eq!(resolve_plugin_path(&root, "a.b", "index.html"), Err(400));
        // traversal in the id
        assert_eq!(resolve_plugin_path(&root, "../etc", "passwd"), Err(400));
    }

    #[test]
    fn resolve_plugin_not_found_returns_404() {
        let root = mktemp_root("notfound");
        assert_eq!(
            resolve_plugin_path(&root, "inexistent", "index.html"),
            Err(404)
        );
    }

    #[test]
    fn resolve_valid_path_ok() {
        let root = mktemp_root("valid");
        mk_plugin(&root, "rag", "index.html", "<h1>ok</h1>");
        let res = resolve_plugin_path(&root, "rag", "/index.html");
        assert!(res.is_ok(), "expected Ok, got {:?}", res);
    }

    #[test]
    fn resolve_traversal_to_parent_rejected() {
        let root = mktemp_root("traversal");
        mk_plugin(&root, "rag", "index.html", "<h1>rag</h1>");
        // File outside the rag/ui scope
        let _ = fs::write(root.join("secret.txt"), "SECRET");
        // Attack: escape with ../../secret.txt (relative to rag/ui → goes up to plugins_root)
        let res = resolve_plugin_path(&root, "rag", "/../../secret.txt");
        assert!(res.is_err(), "traversal allowed: {:?}", res);
    }

    #[test]
    fn resolve_cross_plugin_rejected() {
        let root = mktemp_root("cross");
        mk_plugin(&root, "rag", "index.html", "<h1>rag</h1>");
        mk_plugin(&root, "altre", "secret.html", "SECRET");
        // From rag trying to read altre/ui/secret.html
        let res = resolve_plugin_path(&root, "rag", "/../../altre/ui/secret.html");
        assert!(res.is_err(), "cross-plugin access allowed: {:?}", res);
    }

    // percent-decoding of paths
    #[test]
    fn resolve_percent_encoded_path_ok() {
        let root = mktemp_root("percent");
        mk_plugin(&root, "rag", "my file.html", "ok");
        let res = resolve_plugin_path(&root, "rag", "/my%20file.html");
        assert!(res.is_ok(), "percent-encoded path failed: {:?}", res);
    }

    #[test]
    fn resolve_percent_encoded_unicode_ok() {
        let root = mktemp_root("percent_utf8");
        mk_plugin(&root, "rag", "fòto.png", "ok");
        // ò = U+00F2 = UTF-8 %C3%B2
        let res = resolve_plugin_path(&root, "rag", "/f%C3%B2to.png");
        assert!(res.is_ok(), "UTF-8 percent-encoded failed: {:?}", res);
    }

    // directory-as-file bug regression test
    #[test]
    fn resolve_directory_rejected() {
        let root = mktemp_root("dir_reject");
        mk_plugin(&root, "rag", "index.html", "ok");
        std::fs::create_dir_all(root.join("rag/ui/subdir")).unwrap();
        // Request to directory must be 404, not Ok
        assert_eq!(resolve_plugin_path(&root, "rag", "/subdir"), Err(404));
    }

    // symlink escape (Unix-only)
    #[cfg(unix)]
    #[test]
    fn resolve_symlink_escape_rejected() {
        use std::os::unix::fs::symlink;
        let root = mktemp_root("symlink_escape");
        mk_plugin(&root, "rag", "index.html", "ok");
        // Secret file outside the rag/ui/ scope
        std::fs::write(root.join("secret.txt"), "SECRET").unwrap();
        // Symlink INSIDE ui/ pointing OUTSIDE (to the secret)
        let _ = symlink(root.join("secret.txt"), root.join("rag/ui/evil.html"));
        let res = resolve_plugin_path(&root, "rag", "/evil.html");
        assert!(res.is_err(), "symlink escape allowed: {:?}", res);
    }

    // empty path and slash alone
    #[test]
    fn resolve_empty_path_is_directory_rejected() {
        let root = mktemp_root("empty_path");
        mk_plugin(&root, "rag", "index.html", "ok");
        // "" and "/" resolve to the ui/ directory → must be 404 (is_file check)
        assert_eq!(resolve_plugin_path(&root, "rag", ""), Err(404));
        assert_eq!(resolve_plugin_path(&root, "rag", "/"), Err(404));
    }

    // null byte in the path
    #[test]
    fn resolve_null_byte_path_rejected() {
        let root = mktemp_root("null_byte");
        mk_plugin(&root, "rag", "index.html", "ok");
        let res = resolve_plugin_path(&root, "rag", "/index.html\0evil");
        assert!(res.is_err(), "null byte allowed: {:?}", res);
    }

    // WSH-005 — post-poll decision. Mutation: reverting poll_sidecar_health
    // to the old "same break for success and timeout, then navigate anyway"
    // behaviour corresponds to mapping (false, false) to Navigate — the
    // third assertion catches it.
    #[test]
    fn post_poll_action_never_navigates_on_timeout() {
        assert_eq!(post_poll_action(true, false), PostPollAction::Navigate);
        assert_eq!(post_poll_action(true, true), PostPollAction::DeferToWizard);
        assert_eq!(
            post_poll_action(false, false),
            PostPollAction::StayAndNotify,
            "timeout on a normal run must stay on the splash and notify, never navigate"
        );
        assert_eq!(
            post_poll_action(false, true),
            PostPollAction::StayQuiet,
            "timeout during first run must not clobber the wizard with an event"
        );
    }

    // rate limiter resets per window
    #[test]
    fn rate_limit_per_plugin_allows_under_threshold() {
        // The combined gate (per-plugin + global, WSC-003) returns true under limit.
        // We do not test exact rejection (shared global state).
        assert!(plugin_rate_limits_ok("test_a"));
        assert!(plugin_rate_limits_ok("test_a"));
    }

    #[test]
    fn rate_limit_per_plugin_isolated_between_plugins() {
        // Plugin A and plugin B have independent counters.
        for _ in 0..500 {
            let _ = crate::rate_limit::rate_limit_ok_for("isolated_a");
        }
        // B starts at zero, must not be affected by A's consumption
        assert!(crate::rate_limit::rate_limit_ok_for("isolated_b"));
    }

    // 4 new tests for validate_request
    #[test]
    fn validate_request_get_ok() {
        let uri: tauri::http::Uri = "plugin://rag/index.html".parse().unwrap();
        assert!(validate_request("GET", &uri).is_ok());
    }

    #[test]
    fn validate_request_head_ok() {
        let uri: tauri::http::Uri = "plugin://rag/index.html".parse().unwrap();
        assert!(validate_request("HEAD", &uri).is_ok());
    }

    #[test]
    fn validate_request_post_rejected_405() {
        let uri: tauri::http::Uri = "plugin://rag/index.html".parse().unwrap();
        assert_eq!(validate_request("POST", &uri), Err(405));
        assert_eq!(validate_request("OPTIONS", &uri), Err(405));
        assert_eq!(validate_request("PUT", &uri), Err(405));
        assert_eq!(validate_request("DELETE", &uri), Err(405));
    }

    // query strings are NOT rejected (JS frameworks use ?v=123 cache-bust).
    #[test]
    fn validate_request_accepts_query_string() {
        let uri: tauri::http::Uri = "plugin://rag/x.html?v=123".parse().unwrap();
        assert_eq!(validate_request("GET", &uri), Ok(()));
        let uri: tauri::http::Uri = "plugin://rag/x.html?screen=settings&id=42".parse().unwrap();
        assert_eq!(validate_request("GET", &uri), Ok(()));
    }

    // explicit port is still rejected (surface reduction).
    #[test]
    fn validate_request_rejects_explicit_port() {
        let uri: tauri::http::Uri = "plugin://rag:80/x.html".parse().unwrap();
        assert_eq!(validate_request("GET", &uri), Err(400));
    }

    // Large file regression — the resolver accepts, the handler rejects via cap
    #[test]
    fn resolve_accepts_large_file_but_handler_caps() {
        let root = mktemp_root("large");
        let ui = root.join("rag/ui");
        std::fs::create_dir_all(&ui).unwrap();
        // 11MB file → resolver ok, handler 413
        std::fs::write(ui.join("big.bin"), vec![0u8; 11 * 1024 * 1024]).unwrap();
        let res = resolve_plugin_path(&root, "rag", "/big.bin");
        assert!(res.is_ok(), "resolver must ok (size check is in handler)");
        let meta = std::fs::metadata(res.unwrap()).unwrap();
        assert!(meta.len() > 10 * 1024 * 1024, "file must be >10MB");
    }

    // Plugin integrity tests (ADR-0014 active)

    fn mk_plugin_with_manifest(root: &Path, id: &str, hash: &str) {
        mk_plugin(root, id, "index.html", "hello");
        let manifest = format!(
            "[plugin]\nid = \"{}\"\nversion = \"0.1.0\"\n\n[integrity]\nsha256 = \"{}\"\n",
            id, hash
        );
        fs::write(root.join(id).join("manifest.toml"), manifest).unwrap();
    }

    #[test]
    fn compute_hash_deterministic() {
        let root = mktemp_root("hash_det");
        mk_plugin_with_manifest(&root, "rag", "placeholder");
        let h1 = compute_plugin_hash(&root.join("rag")).unwrap();
        let h2 = compute_plugin_hash(&root.join("rag")).unwrap();
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64, "sha256 hex = 64 chars");
    }

    #[test]
    fn compute_hash_ignores_integrity_section() {
        // The hash MUST NOT depend on the integrity.sha256 field (circularity).
        let root = mktemp_root("hash_canon");
        mk_plugin_with_manifest(&root, "rag", "aaaaaaaaaaaaaaaa");
        let h1 = compute_plugin_hash(&root.join("rag")).unwrap();
        mk_plugin_with_manifest(&root, "rag", "bbbbbbbbbbbbbbbb");
        let h2 = compute_plugin_hash(&root.join("rag")).unwrap();
        assert_eq!(h1, h2, "change to [integrity] must not affect the hash");
    }

    #[test]
    fn compute_hash_changes_when_content_changes() {
        let root = mktemp_root("hash_diff");
        mk_plugin_with_manifest(&root, "rag", "x");
        let h1 = compute_plugin_hash(&root.join("rag")).unwrap();
        // Modify one byte of the content
        fs::write(root.join("rag/ui/index.html"), "hello!").unwrap();
        let h2 = compute_plugin_hash(&root.join("rag")).unwrap();
        assert_ne!(h1, h2, "content change must change the hash");
    }

    #[test]
    fn verify_integrity_valid_passes() {
        let root = mktemp_root("verify_ok");
        mk_plugin_with_manifest(&root, "pluga", "placeholder");
        let actual = compute_plugin_hash(&root.join("pluga")).unwrap();
        mk_plugin_with_manifest(&root, "pluga", &actual);
        let res = verify_plugin_integrity("pluga", &root);
        assert!(res.is_ok(), "correct hash must pass: {:?}", res);
    }

    // The verify_plugin_integrity tests expecting Err(403) require STRICT_INTEGRITY=true
    // (release only). In debug, the function returns Ok(()) to avoid friction each-edit.
    #[cfg(not(debug_assertions))]
    #[test]
    fn verify_integrity_mismatch_rejected() {
        let root = mktemp_root("verify_mismatch");
        // different plugin id to avoid cache collision with other tests
        mk_plugin_with_manifest(
            &root,
            "plugb",
            "0000000000000000000000000000000000000000000000000000000000000000",
        );
        let res = verify_plugin_integrity("plugb", &root);
        assert_eq!(res, Err(403), "incorrect hash must return 403");
    }

    #[cfg(not(debug_assertions))]
    #[test]
    fn verify_integrity_no_manifest_rejected() {
        let root = mktemp_root("verify_nomanifest");
        mk_plugin(&root, "plugc", "index.html", "hello");
        // without manifest.toml
        let res = verify_plugin_integrity("plugc", &root);
        assert_eq!(res, Err(403), "plugin without manifest must fail");
    }

    #[cfg(not(debug_assertions))]
    #[test]
    fn verify_integrity_empty_hash_rejected() {
        let root = mktemp_root("verify_emptyhash");
        mk_plugin_with_manifest(&root, "plugd", "");
        let res = verify_plugin_integrity("plugd", &root);
        assert_eq!(res, Err(403), "empty hash must fail");
    }

    #[cfg(not(debug_assertions))]
    #[test]
    fn verify_integrity_short_hash_rejected() {
        let root = mktemp_root("verify_shorthash");
        mk_plugin_with_manifest(&root, "pluge", "abc123"); // < 64 chars
        let res = verify_plugin_integrity("pluge", &root);
        assert_eq!(res, Err(403), "hash with incorrect length must fail");
    }

    #[test]
    fn compute_hash_handles_subdirs() {
        let root = mktemp_root("hash_subdirs");
        mk_plugin(&root, "rag", "index.html", "root");
        mk_plugin(&root, "rag", "sub/nested.css", "body{}");
        let manifest = "[plugin]\nid = \"rag\"\n[integrity]\nsha256 = \"x\"\n";
        fs::write(root.join("rag/manifest.toml"), manifest).unwrap();
        let h = compute_plugin_hash(&root.join("rag")).unwrap();
        assert_eq!(h.len(), 64);
    }

    // read-with-cap pattern (no TOCTOU metadata→read).
    // Validates that File::open + take(MAX+1) + read_to_end truncates to MAX+1 bytes
    // regardless of the actual file size.
    #[test]
    fn read_with_cap_truncates_at_limit_plus_one() {
        use std::io::Write;
        let root = mktemp_root("read_cap");
        fs::create_dir_all(&root).unwrap();
        let file_path = root.join("big.bin");
        let mut f = fs::File::create(&file_path).unwrap();
        // Writes 1MB + 100 bytes — larger than the test cap (1KB).
        let chunk = vec![0u8; 1024];
        for _ in 0..1024 {
            f.write_all(&chunk).unwrap();
        }
        f.write_all(&[0u8; 100]).unwrap();
        drop(f);

        const CAP: u64 = 1024;
        let open = fs::File::open(&file_path).unwrap();
        let mut buf = Vec::new();
        let n = open.take(CAP + 1).read_to_end(&mut buf).unwrap() as u64;
        assert_eq!(n, CAP + 1, "take(CAP+1) must read exactly CAP+1 bytes");
        assert!(n > CAP, "handler detectaria oversize");
    }

    // C01 (2026-04-21) — TOCTOU edit in-place of an existing file
    // without mutating the parent directory's mtime. This is exactly the
    // vector the old algorithm (CacheEntry { mtime }) allowed:
    // APFS/NTFS/POSIX do NOT update dir mtime on in-place write, so
    // the cache hit kept serving the verified verdict. The fix
    // re-computes the hash on every request.
    //
    // Release-only: in debug STRICT_INTEGRITY=false and verify returns Ok(())
    // (friction-each-edit mitigation).
    #[cfg(not(debug_assertions))]
    #[test]
    fn toctou_edit_in_place_detected() {
        let root = mktemp_root("toctou_inplace");
        // Setup: plugin with manifest consistent with initial content.
        mk_plugin(&root, "plugx", "index.html", "original content");
        let hash = compute_plugin_hash(&root.join("plugx")).unwrap();
        mk_plugin_with_manifest(&root, "plugx", &hash);
        // We rewrite the same initial content because
        // `mk_plugin_with_manifest` overwrites `ui/index.html` with "hello".
        fs::write(root.join("plugx/ui/index.html"), "original content").unwrap();
        // We recompute the hash with real content and update it in the manifest
        // to ensure consistency (mk_plugin_with_manifest should keep it
        // but we do it explicitly to harden the test against future refactors).
        let hash = compute_plugin_hash(&root.join("plugx")).unwrap();
        mk_plugin_with_manifest(&root, "plugx", &hash);
        fs::write(root.join("plugx/ui/index.html"), "original content").unwrap();

        // First verify OK → cache populated with known_hash.
        assert_eq!(
            verify_plugin_integrity("plugx", &root),
            Ok(()),
            "baseline verify must pass"
        );

        // Edit in-place without adding/deleting/renaming entries → parent dir mtime
        // remains invariant on most FS (APFS empirically).
        // No sleep changes this; we wait for robustness clock resolution.
        std::thread::sleep(std::time::Duration::from_millis(10));
        fs::write(
            root.join("plugx/ui/index.html"),
            "<script>MALICIOUS</script>",
        )
        .unwrap();

        // ✅ If the old algorithm (cache by dir mtime) were used, this would give
        // Ok(()) falsely. The C01 fix requires 403 for hash mismatch.
        assert_eq!(
            verify_plugin_integrity("plugx", &root),
            Err(403),
            "in-place edit of an existing file must be detected (C01 TOCTOU)"
        );
    }

    // B5 (2026-04-21) — TOCTOU verify→serve atomic snapshot.
    //
    // Security PoC (70.5% reproducible hit-rate): the handler did
    // verify_plugin_integrity (one FS read) + File::open + read_to_end
    // (second read). Between the two, a local attacker with write access to
    // plugins-dev/<id>/ could replace content and the serve returned bytes
    // different from what the hash had just verified.
    //
    // Fix: verify_and_load_plugin_asset does verify + load in ONE atomic snapshot
    // (opens all fds BEFORE any read, hashes from the snapshot,
    // returns the bytes of the requested file FROM THE SAME snapshot).
    //
    // Security invariant: if Ok(bytes), the bytes correspond to the hash that has
    // passed verification. No Ok with bytes different from the manifest.
    //
    // Release-only (STRICT_INTEGRITY=true).
    #[cfg(not(debug_assertions))]
    #[test]
    fn b5_verify_and_load_atomic_snapshot_no_bypass() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let root = mktemp_root("b5_atomic_snapshot");

        // Setup: plugin with benign content and hash consistent with manifest.
        mk_plugin(&root, "target", "index.html", "benign_content");
        let h = crate::integrity::compute_plugin_hash(&root.join("target")).unwrap();
        mk_plugin_with_manifest(&root, "target", &h);
        fs::write(root.join("target/ui/index.html"), "benign_content").unwrap();
        let h = crate::integrity::compute_plugin_hash(&root.join("target")).unwrap();
        mk_plugin_with_manifest(&root, "target", &h);
        fs::write(root.join("target/ui/index.html"), "benign_content").unwrap();

        // Baseline: verify_and_load returns correct bytes without attacker.
        let baseline =
            crate::integrity::verify_and_load_plugin_asset("target", &root, "ui/index.html")
                .unwrap();
        assert_eq!(baseline, b"benign_content");

        // Attacker thread: spin-write alternating between benign and "MAL" (short
        // malicious content to speed up writes) to the same file. With
        // the old algorithm, a window between verify and serve allowed the
        // serve to read "MAL" while the hash had seen "benign_content".
        let attacker_stop = Arc::new(AtomicBool::new(false));
        let stop_flag = attacker_stop.clone();
        let target_file = root.join("target/ui/index.html");
        let attacker = std::thread::spawn(move || {
            let mut toggle = false;
            while !stop_flag.load(Ordering::Relaxed) {
                let content: &[u8] = if toggle { b"benign_content" } else { b"MAL" };
                let _ = fs::write(&target_file, content);
                toggle = !toggle;
                std::thread::yield_now();
            }
        });

        // Victim: 500 requests to verify_and_load. We count:
        //   - Ok with bytes == "benign_content" → OK (hash match, snapshot consistent)
        //   - Ok with bytes != "benign_content" → BYPASS ❌ (invariant broken)
        //   - Err(403) → OK (hash mismatch detected)
        //   - Err(other) → acceptable (race I/O at open)
        let mut ok_benign = 0;
        let mut ok_bypass = 0;
        let mut err_403 = 0;
        let mut err_other = 0;
        for _ in 0..500 {
            match crate::integrity::verify_and_load_plugin_asset("target", &root, "ui/index.html") {
                Ok(bytes) if bytes == b"benign_content" => ok_benign += 1,
                Ok(_) => ok_bypass += 1,
                Err(403) => err_403 += 1,
                Err(_) => err_other += 1,
            }
        }

        attacker_stop.store(true, Ordering::Relaxed);
        let _ = attacker.join();

        eprintln!(
            "B5 stats: ok_benign={} ok_bypass={} err_403={} err_other={}",
            ok_benign, ok_bypass, err_403, err_other
        );

        // Key invariant: ZERO serves with bytes that do not correspond to the verified hash.
        // With the old algorithm, ok_bypass ≈ 70% of 500 = ~350.
        // With the atomic snapshot it must be 0.
        assert_eq!(
            ok_bypass, 0,
            "B5 BYPASS: {} serves retornen bytes diferents del que el hash va verificar",
            ok_bypass
        );

        // Sanity: the test must actually exercise the race (some 403s expected
        // if the attacker captured the snapshot with "MAL"). If all were
        // ok_benign, the test would not really exercise the vector.
        assert!(
            err_403 + ok_benign > 0,
            "test didn't exercise any real request"
        );
    }

    // B5 + B6: verifies that the function rejects plugins with files > MAX_HASH_FILE_BYTES.
    #[cfg(not(debug_assertions))]
    #[test]
    fn b6_hash_per_file_cap_enforced() {
        let root = mktemp_root("b6_per_file_cap");
        mk_plugin(&root, "huge", "index.html", "small");
        // Writes a file > 10 MB (MAX_HASH_FILE_BYTES)
        let big_path = root.join("huge/ui/big.bin");
        let big = vec![0u8; (crate::integrity::MAX_HASH_FILE_BYTES as usize) + 10];
        fs::write(&big_path, &big).unwrap();
        mk_plugin_with_manifest(&root, "huge", "x");

        // verify_and_load_plugin_asset must return 413 for large file
        let res = crate::integrity::verify_and_load_plugin_asset("huge", &root, "ui/index.html");
        assert_eq!(res, Err(413), "file > MAX_HASH_FILE_BYTES must return 413");
    }

    // B6 gap fix (2026-04-22): plugin with all files
    // individually within the per-file cap, but with aggregate sum >
    // MAX_HASH_TOTAL_BYTES (50 MB), must return Err(413). OOM prevention
    // via "thousand small files" that cumulatively saturate RAM.
    //
    // Adversarial review reported: the total cap was IMPLEMENTED in the code
    // (verify_and_load_plugin_asset returns Err(413) if total_bytes_read
    // > MAX_HASH_TOTAL_BYTES) but WITHOUT a regression test. If someone
    // removes the total check, no test catches the regression.
    //
    // Test strategy: 6 files × 9 MB each = 54 MB total,
    // exceeding MAX_HASH_TOTAL_BYTES (50 MB) while keeping each file
    // individually below MAX_HASH_FILE_BYTES (10 MB) → only the total cap can catch it.
    #[cfg(not(debug_assertions))]
    #[test]
    fn b6_hash_total_cap_enforced() {
        let root = mktemp_root("b6_total_cap");
        mk_plugin(&root, "multibig", "index.html", "small");

        // 6 files × 9 MB = 54 MB > MAX_HASH_TOTAL_BYTES (50 MB),
        // each file individually < MAX_HASH_FILE_BYTES (10 MB).
        let per_file_size = 9 * 1024 * 1024; // 9 MB
        for i in 0..6 {
            let path = root.join(format!("multibig/ui/chunk_{i}.bin"));
            fs::write(&path, vec![0u8; per_file_size]).unwrap();
        }
        mk_plugin_with_manifest(&root, "multibig", "x");

        // Must return 413 (total cap exceeded) — even though no individual file
        // exceeds it. If someone removes the MAX_HASH_TOTAL_BYTES check from
        // verify_and_load_plugin_asset, this test fails.
        let res =
            crate::integrity::verify_and_load_plugin_asset("multibig", &root, "ui/index.html");
        assert_eq!(
            res,
            Err(413),
            "6 files × 9MB = 54MB > MAX_HASH_TOTAL_BYTES must return 413"
        );
    }

    // reentrancy depth guard: HANDLER_DEPTH increments/decrements correctly
    // via DepthGuard (RAII). Depth does not grow without bound.
    #[test]
    fn reentrancy_depth_tracked_and_reset() {
        // Baseline: depth 0 at start
        HANDLER_DEPTH.with(|d| d.set(0));
        assert_eq!(HANDLER_DEPTH.with(|d| d.get()), 0);

        // Simulate handler entry (without calling the whole fn):
        struct DepthGuard;
        impl Drop for DepthGuard {
            fn drop(&mut self) {
                HANDLER_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
            }
        }
        {
            HANDLER_DEPTH.with(|d| d.set(d.get() + 1));
            let _g = DepthGuard;
            assert_eq!(HANDLER_DEPTH.with(|d| d.get()), 1);
            // Nested (reentrant)
            {
                HANDLER_DEPTH.with(|d| d.set(d.get() + 1));
                let _g2 = DepthGuard;
                assert_eq!(HANDLER_DEPTH.with(|d| d.get()), 2);
            }
            // After inner drop
            assert_eq!(HANDLER_DEPTH.with(|d| d.get()), 1);
        }
        // After outer drop: back to 0
        assert_eq!(HANDLER_DEPTH.with(|d| d.get()), 0);
    }

    #[test]
    fn reentrancy_max_depth_limit() {
        // MAX_HANDLER_DEPTH = 4 → requests with depth >= 4 are rejected with 429.
        // Here we only verify the value; the rejection logic is in the handler.
        assert_eq!(MAX_HANDLER_DEPTH, 4, "MAX_HANDLER_DEPTH constant check");
    }

    // AuthToken generates UUID v4 (128 bits) unique per launch.
    #[test]
    fn auth_token_generate_is_uuid_v4() {
        let t = AuthToken::generate();
        // UUID v4 format: xxxxxxxx-xxxx-4xxx-yxxx-xxxxxxxxxxxx (36 chars incl. hyphens)
        assert_eq!(t.0.len(), 36);
        assert_eq!(t.0.chars().filter(|&c| c == '-').count(), 4);
        // Version digit at pos 14: '4' for UUID v4
        assert_eq!(t.0.chars().nth(14).unwrap(), '4');
    }

    #[test]
    fn auth_token_each_generate_is_distinct() {
        // Two consecutive generations must produce different tokens
        // (128 bits entropy — collisions statistically impossible).
        let a = AuthToken::generate();
        let b = AuthToken::generate();
        assert_ne!(a.0, b.0, "tokens must be unique per launch");
    }

    // LRU cap bounded (RATE_LIMITERS does not grow without bound)
    #[test]
    fn rate_limiter_lru_bounded() {
        // Insert 600 different IDs (> RATE_LIMIT_LRU_CAP = 500)
        for i in 0..600 {
            let id = format!("plugin{:04}", i);
            let _ = crate::rate_limit::rate_limit_ok_for(&id);
        }
        let guard = rate_limiters().lock().unwrap();
        assert!(
            guard.len() <= RATE_LIMIT_LRU_CAP,
            "LRU cap not respected: {} > {}",
            guard.len(),
            RATE_LIMIT_LRU_CAP
        );
    }

    // handler_pool bounded (no 1-thread-per-request).
    #[test]
    fn handler_pool_bounded() {
        let pool = handler_pool();
        // Exhaust the pool with 8+N blocking jobs
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(9));
        for _ in 0..8 {
            let c = counter.clone();
            let b = barrier.clone();
            pool.execute(move || {
                c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                b.wait(); // blocks until all 8 + main are present
            });
        }
        // Give time for all 8 workers to reach the barrier
        std::thread::sleep(std::time::Duration::from_millis(50));
        // Pool has exactly 8 active workers — no more can run until these finish
        assert_eq!(pool.active_count(), 8);
        assert_eq!(
            pool.max_count(),
            8,
            "pool max 8 threads (prevents thread-bomb)"
        );
        barrier.wait(); // unblocks all workers
        pool.join();
    }

    // re-check pattern prevents inconsistent overwrites in races
    #[cfg(not(debug_assertions))]
    #[test]
    fn concurrent_verify_determinism() {
        use std::sync::Arc;
        let root = Arc::new(mktemp_root("concurrent_verify"));
        let hash = {
            let tmp = root.clone();
            mk_plugin(&tmp, "plugc", "index.html", "hello");
            let manifest = format!(
                "[plugin]\nid = \"plugc\"\nversion = \"0.1.0\"\n\n[integrity]\nsha256 = \"{}\"\n",
                "0".repeat(64)
            );
            fs::write(tmp.join("plugc/manifest.toml"), manifest).unwrap();
            let actual = compute_plugin_hash(&tmp.join("plugc")).unwrap();
            mk_plugin_with_manifest(&tmp, "plugc", &actual);
            actual
        };
        // 10 threads verifying concurrently → all must return Ok(())
        let mut handles = vec![];
        for _ in 0..10 {
            let r = root.clone();
            handles.push(std::thread::spawn(move || {
                verify_plugin_integrity("plugc", &r)
            }));
        }
        for h in handles {
            assert_eq!(h.join().unwrap(), Ok(()), "concurrent verify inconsistent");
        }
        // Cache contains ONE consistent entry (observability-only, C01).
        let guard = verified_plugins().lock().unwrap();
        let entry = guard.peek("plugc").expect("expected cache hit");
        // Sanity: the cached known_hash matches the real hash.
        assert_eq!(
            entry.known_hash, hash,
            "cache observability: known_hash must match the real hash"
        );
    }

    // errors are NOT persisted in the cache (auto-recovery).
    #[cfg(not(debug_assertions))]
    #[test]
    fn cache_does_not_persist_errors() {
        let root = mktemp_root("no_err_cache");
        // First request: manifest with incorrect hash → Err(403)
        mk_plugin_with_manifest(&root, "plugr", "0".repeat(64).as_str());
        assert_eq!(verify_plugin_integrity("plugr", &root), Err(403));

        // Fix the manifest with the correct hash
        std::thread::sleep(std::time::Duration::from_millis(10));
        let correct = compute_plugin_hash(&root.join("plugr")).unwrap();
        mk_plugin_with_manifest(&root, "plugr", &correct);
        // Second request must PASS — the error has NOT been cached.
        assert_eq!(
            verify_plugin_integrity("plugr", &root),
            Ok(()),
            "transient error must not be persisted in the cache"
        );
    }

    // ─────────────────────────────────────────────────────────────────
    // C06 / C14 / C30 (2026-04-21) — regression tests
    // ─────────────────────────────────────────────────────────────────

    // C06 — extract_plugin_id_from_uri pure function (pre-queue).
    #[test]
    fn extract_plugin_id_basic() {
        assert_eq!(
            extract_plugin_id_from_uri("plugin://rag/index.html"),
            Some("rag".to_string())
        );
        assert_eq!(
            extract_plugin_id_from_uri("plugin://my-plugin/assets/app.js"),
            Some("my-plugin".to_string())
        );
        // without trailing path
        assert_eq!(
            extract_plugin_id_from_uri("plugin://rag"),
            Some("rag".to_string())
        );
    }

    #[test]
    fn extract_plugin_id_rejects_missing_host() {
        // plugin:/// — path without host
        assert_eq!(extract_plugin_id_from_uri("plugin:///foo"), None);
        // different scheme
        assert_eq!(extract_plugin_id_from_uri("https://rag/foo"), None);
        // string random
        assert_eq!(extract_plugin_id_from_uri(""), None);
        assert_eq!(extract_plugin_id_from_uri("plugin"), None);
    }

    // ─────────────────────────────────────────────────────────────────
    // C06 legacy tests (`max_queued_constant_sanity`,
    // `handler_pool_queue_count_accessible`) removed
    // (2026-04-21). Both were theatre:
    //   - `max_queued_constant_sanity` only verified `MAX_QUEUED == 256`
    //     literally (no real behavior).
    //   - `handler_pool_queue_count_accessible` asserted `usize < usize::MAX`
    //     (tautology) and enqueued a trivial job that did not touch MAX_QUEUED.
    // Both have been subsumed by `b3_queue_bound_atomic_race` (further
    // down in this file) which exercises the real CAS with N = MAX_QUEUED+100
    // concurrent threads and verifies the 4 invariants of the B3 fix.
    // ─────────────────────────────────────────────────────────────────

    // ─────────────────────────────────────────────────────────────────
    // C14 (2026-04-21) — dialog guard real mutation test.
    //
    // The old `dialog_showing_guard_semantics` (removed) only exercised
    // `AtomicBool::swap` from stdlib. If someone removed the swap from `graceful_quit`
    // the test kept passing (verified by mutation testing: removing the
    // guard from `graceful_quit` → test ok).
    //
    // Fix: extracted `graceful_quit_try_acquire()` in `lifecycle.rs` as a
    // pure helper (returns `!DIALOG_SHOWING.swap(true, AcqRel)`). `graceful_quit`
    // now calls `if !graceful_quit_try_acquire() { return; }`. This test
    // launches N threads in parallel via `Barrier` and asserts that only ONE
    // thread returns `true`.
    //
    // Mutation testing (verification 2026-04-22):
    //   - If someone replaces `swap(true, AcqRel)` with unconditional `store(true)`
    //     (always-true return) → test fails `acquired != 1` (256 true returns).
    //   - If someone replaces with a separate racy `load+store` pattern → the test
    //     does NOT catch it reliably (confirmed: 10/10 passes with racy
    //     pattern on macOS M4 + 256 threads + Barrier). This limitation is
    //     inherent to observable contention without a formal model-checker
    //     (e.g. `loom`) that explores all possible interleavings.
    //
    //   To catch subtle load+store mutations: future options:
    //     1. Integrate `loom` with `lifecycle` code for model-checking
    //     2. Increase N to 10000+ threads (high CI cost, reliability not guaranteed)
    //     3. Add `cargo-mutants` fuzzing test that marks load+store
    //        as an uncaught variant and documents it
    //
    // Debug + release: the test is deterministic for the "correct" pattern (an
    // atomic CAS never lets > 1 thread win), does not depend on timing for false
    // positives. Hence no `cfg(not(debug_assertions))` — coverage in both
    // configurations.
    // ─────────────────────────────────────────────────────────────────
    #[test]
    fn t1_dialog_guard_only_one_acquires_under_concurrency() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        // PRIVATE flag — this test used to `store(false)` the process-wide
        // `DIALOG_SHOWING` ("Reset — other lifecycle tests may have left it
        // true"), which is precisely the race: `lifecycle::tests` serialises its
        // five dialog tests on `DIALOG_TEST_GUARD`, but that mutex is private to
        // that module and this test — living in `lib.rs` — could not take it. It
        // therefore cleared and set the singleton underneath a test that held the
        // lock: measured 2 reds in 300 loaded runs, always killing
        // `lifecycle::tests::t_no_shutdown_allows_first_dialog`.
        //
        // Owning the flag removes the shared state instead of widening the lock.
        // The logic under test is still the real one (`dialog_try_acquire_in` is
        // what `graceful_quit_try_acquire` calls), never a copy.
        let flag = AtomicBool::new(false);

        // How the contention is produced, and why it is not a `Barrier` any more.
        //
        // The previous shape (256 threads on one `Barrier::wait()`) was a
        // PROBABILISTIC guard: a barrier releases its waiters by waking parked
        // threads, which the OS does one after another, so the two-instruction
        // window of a broken CAS (`load` then `store` instead of `swap`) almost
        // never had two threads inside it. Measured on this machine: with the
        // logic mutant applied, that version passed 20 of 20 unloaded runs — the
        // test promised "any mutation causes > 1 thread to win" and did not
        // deliver it.
        //
        // Two changes make it deterministic:
        //   · ROUNDS instead of one shot — each round is an independent chance to
        //     catch the window, so the probability of missing it decays as p^N;
        //   · a SPIN start instead of a barrier — the workers are already on CPU
        //     burning `spin_loop()` when the round counter flips, so they enter
        //     the acquire within nanoseconds of each other instead of waiting to
        //     be woken up one by one.
        // Fewer threads than before (8, not 256) on purpose: 256 threads on 16
        // cores cannot be simultaneous, they queue. Threads that actually run at
        // the same instant are what creates contention.
        //
        // Invariant checked: EXACTLY ONE winner per round, so `winners == ROUNDS`.
        // Under a broken CAS some round hands the flag to two threads and the
        // total overshoots.
        const THREADS: usize = 8;
        const ROUNDS: usize = 500;
        let round = AtomicUsize::new(0); // 0 = not started; round r runs at r
        let done = AtomicUsize::new(0); // workers that finished the current round
        let acquired = AtomicUsize::new(0);

        std::thread::scope(|s| {
            for _ in 0..THREADS {
                s.spawn(|| {
                    for r in 1..=ROUNDS {
                        while round.load(Ordering::Acquire) < r {
                            std::hint::spin_loop();
                        }
                        if dialog_try_acquire_in(&flag) {
                            acquired.fetch_add(1, Ordering::Relaxed);
                        }
                        done.fetch_add(1, Ordering::Release);
                    }
                });
            }
            for r in 1..=ROUNDS {
                // Safe to reset: every worker finished round r-1 before we got
                // here, and none can start round r until `round` is bumped last.
                flag.store(false, Ordering::Release);
                done.store(0, Ordering::Release);
                round.store(r, Ordering::Release);
                while done.load(Ordering::Acquire) < THREADS {
                    std::hint::spin_loop();
                }
            }
        });

        let acquired_n = acquired.load(Ordering::Relaxed);
        let expected = ROUNDS;
        assert_eq!(
            acquired_n, expected,
            "C14 guard violated: {acquired_n} acquires over {expected} rounds \
            (expected exactly one winner per round). If GREATER, swap(true, AcqRel) has been \
            replaced by a non-atomic operation and two threads got inside the window in some \
            round. If SMALLER, dialog_try_acquire_in returns false to everybody (guard \
            permanently blocked)."
        );
        // No cleanup: the flag was local and dies here. Nothing global was touched.
    }

    // C30 — rate_limit_ok_for does not alloc per request: the cache must only
    // contain ONE entry for the same plugin even if called N times.
    // We cannot observe allocs directly (Rust has no counter) but we can
    // verify that `contains(id)` stays true after 1000 calls
    // (not evicted or duplicated).
    #[test]
    fn rate_limit_no_duplicate_entry_same_id() {
        let id = "c30_stable_id_unique_xyz";
        for _ in 0..1000 {
            let _ = crate::rate_limit::rate_limit_ok_for(id);
        }
        let guard = rate_limiters().lock().unwrap();
        // Exactly 1 entry for our id (no duplication from repeated alloc).
        assert!(
            guard.contains(id),
            "rate_limiter must maintain a unique entry for the same id"
        );
    }

    // C41 — finish_with_timing does not panic if started is in the past relative to target
    // (unlikely but defends against monotonic clock drift on hibernate).
    #[test]
    fn finish_with_timing_no_panic_on_past_target() {
        use std::thread::sleep;
        use std::time::{Duration, Instant};
        // Simulate started well in the past so elapsed > TARGET (50ms).
        let started = Instant::now() - Duration::from_millis(200);
        // Must return immediately without panic (checked_sub = None → no sleep).
        // The test only verifies that it does NOT panic.
        let resp = err_response(200, b"ok");
        let _ = finish_with_timing(resp, started);
        sleep(Duration::from_millis(1));
    }

    // C51 — content_type case sanity (headers test already covered in other tests).
    #[test]
    fn content_type_modern_formats_sanity() {
        // Re-verify that critical modern extensions are still mapped.
        assert_eq!(content_type_for("x.woff2"), "font/woff2");
        assert_eq!(content_type_for("x.avif"), "image/avif");
        assert_eq!(content_type_for("x.wasm"), "application/wasm");
    }

    // B3 test (b3_queue_bound_atomic_race) has been moved to `handler.rs` (mod tests)
    // — 2026-05-08. PENDING_COUNT and try_acquire_pending_slot live in handler.

    // ─────────────────────────────────────────────────────────────────
    // Z3 (2026-04-21) — err_response defensive headers
    // ─────────────────────────────────────────────────────────────────
    //
    // Security review (Z3/B5): err_response emitted errors 400/403/404/
    // 413/429/503 WITHOUT defensive headers. Fix adds Content-Type, nosniff,
    // Cache-Control, CSP (default-src 'none'; frame-ancestors 'none'),
    // Permissions-Policy, Referrer-Policy, X-Frame-Options DENY, ACAO null.
    //
    // Mutation: if someone removes the headers from err_response, these tests fail.

    #[test]
    fn err_response_has_security_headers() {
        let resp = err_response(404, b"not found");
        let headers = resp.headers();

        assert!(
            headers.get("Content-Type").is_some(),
            "Content-Type header required"
        );
        assert_eq!(
            headers.get("X-Content-Type-Options").unwrap(),
            "nosniff",
            "X-Content-Type-Options nosniff required"
        );
        assert_eq!(
            headers.get("Cache-Control").unwrap(),
            "no-store",
            "Cache-Control no-store required (error responses must not be cached)"
        );
        assert_eq!(
            headers.get("Content-Security-Policy").unwrap(),
            "default-src 'none'; frame-ancestors 'none'",
            "CSP default-src 'none' + frame-ancestors 'none' required on errors"
        );
        assert!(
            headers.get("Permissions-Policy").is_some(),
            "Permissions-Policy required"
        );
        assert_eq!(
            headers.get("Referrer-Policy").unwrap(),
            "no-referrer",
            "Referrer-Policy no-referrer required"
        );
        assert_eq!(
            headers.get("X-Frame-Options").unwrap(),
            "DENY",
            "X-Frame-Options DENY required (block framing of errors)"
        );
        assert_eq!(
            headers.get("Access-Control-Allow-Origin").unwrap(),
            "null",
            "ACAO null required"
        );
    }

    #[test]
    fn err_response_different_status_same_headers() {
        // All status codes that err_response can emit from the handler
        // must have the same defensive headers.
        for status in [400_u16, 403, 404, 405, 413, 429, 500, 503] {
            let resp = err_response(status, b"err");
            assert_eq!(resp.status().as_u16(), status, "status code mismatch");
            let h = resp.headers();
            assert!(
                h.get("X-Content-Type-Options").is_some(),
                "missing nosniff for status {status}"
            );
            assert!(
                h.get("Content-Security-Policy").is_some(),
                "missing CSP for status {status}"
            );
            assert!(
                h.get("X-Frame-Options").is_some(),
                "missing X-Frame-Options for status {status}"
            );
            assert!(
                h.get("Cache-Control").is_some(),
                "missing Cache-Control for status {status}"
            );
        }
    }

    #[test]
    fn err_response_preserves_body_bytes() {
        // Regression: headers must not corrupt the body — the caller
        // trusts that the payload is transmitted intact.
        let body: &[u8] = b"bad request";
        let resp = err_response(400, body);
        assert_eq!(resp.body().as_slice(), body);
    }

    // ─────────────────────────────────────────────────────────────────
    // Per-file integrity (ADR-0014 v2) — TDD tests written before implementation.
    // All tests below will FAIL until the new functions are implemented.
    // TDD plan (ADR-0014 v2).
    // ─────────────────────────────────────────────────────────────────

    #[test]
    fn per_file_compute_file_hash_is_sha256_hex() {
        use crate::integrity::compute_file_hash;
        let h = compute_file_hash(b"hello");
        assert_eq!(h.len(), 64, "sha256 hex must be 64 chars");
        assert_eq!(
            h,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn per_file_manifest_hash_excludes_itself() {
        use crate::integrity::compute_manifest_hash;
        let m1 = "[plugin]\nid=\"x\"\n[integrity]\nmanifest_sha256 = \"IGNORE_ME\"\n[integrity.files]\n\"a.html\" = \"abc\"\n";
        let m2 = "[plugin]\nid=\"x\"\n[integrity]\nmanifest_sha256 = \"DIFFERENT\"\n[integrity.files]\n\"a.html\" = \"abc\"\n";
        let h1 = compute_manifest_hash(m1).unwrap();
        let h2 = compute_manifest_hash(m2).unwrap();
        assert_eq!(
            h1, h2,
            "manifest_sha256 field must be excluded from its own hash"
        );
    }

    #[test]
    fn per_file_manifest_hash_changes_when_files_change() {
        use crate::integrity::compute_manifest_hash;
        let m1 = "[plugin]\nid=\"x\"\n[integrity]\nmanifest_sha256=\"x\"\n[integrity.files]\n\"a.html\" = \"hash1\"\n";
        let m2 = "[plugin]\nid=\"x\"\n[integrity]\nmanifest_sha256=\"x\"\n[integrity.files]\n\"a.html\" = \"hash2\"\n";
        let h1 = compute_manifest_hash(m1).unwrap();
        let h2 = compute_manifest_hash(m2).unwrap();
        assert_ne!(
            h1, h2,
            "different file hashes must produce different manifest hash"
        );
    }

    #[test]
    fn per_file_detect_format_new() {
        use crate::integrity::{detect_integrity_format, IntegrityFormat};
        let m = "[integrity]\nmanifest_sha256 = \"abc\"\n[integrity.files]\n\"x.html\" = \"def\"\n";
        assert!(matches!(
            detect_integrity_format(m),
            IntegrityFormat::PerFile(_)
        ));
    }

    #[test]
    fn per_file_detect_format_legacy() {
        use crate::integrity::{detect_integrity_format, IntegrityFormat};
        let m = "[integrity]\nsha256 = \"abc123\"\n";
        match detect_integrity_format(m) {
            IntegrityFormat::DirectoryHash(h) => assert_eq!(h, "abc123"),
            _ => panic!("expected DirectoryHash"),
        }
    }

    #[test]
    fn per_file_manifest_hash_deterministic_key_order() {
        use crate::integrity::compute_manifest_hash;
        let m1 = "[integrity]\nmanifest_sha256=\"x\"\n[integrity.files]\n\"z.html\" = \"hz\"\n\"a.html\" = \"ha\"\n";
        let m2 = "[integrity]\nmanifest_sha256=\"x\"\n[integrity.files]\n\"a.html\" = \"ha\"\n\"z.html\" = \"hz\"\n";
        let h1 = compute_manifest_hash(m1).unwrap();
        let h2 = compute_manifest_hash(m2).unwrap();
        assert_eq!(h1, h2, "key order in manifest must not affect hash");
    }

    #[cfg(not(debug_assertions))]
    #[test]
    fn per_file_verify_manifest_integrity_valid_passes() {
        use crate::integrity::{verify_manifest_integrity, write_per_file_manifest};
        let root = mktemp_root("perfile_manifest_ok");
        mk_plugin(&root, "plug", "index.html", "hello"); // creates root/plug/ui/index.html
        write_per_file_manifest(&root, "plug").unwrap();
        let result = verify_manifest_integrity(&root.join("plug"));
        assert!(result.is_ok(), "valid manifest must pass: {:?}", result);
        let files = result.unwrap();
        // mk_plugin creates root/plug/ui/index.html — rel_path is "ui/index.html"
        assert!(
            files.contains_key("ui/index.html"),
            "ui/index.html must be in files map"
        );
    }

    #[cfg(not(debug_assertions))]
    #[test]
    fn per_file_verify_manifest_integrity_tampered_rejected() {
        use crate::integrity::{verify_manifest_integrity, write_per_file_manifest};
        let root = mktemp_root("perfile_manifest_tampered");
        mk_plugin(&root, "plug", "index.html", "hello");
        write_per_file_manifest(&root, "plug").unwrap();
        // Corrupt manifest_sha256
        let manifest_path = root.join("plug/manifest.toml");
        let content = fs::read_to_string(&manifest_path).unwrap();
        let corrupted = content.replace(
            content.lines().find(|l| l.contains("manifest_sha256")).unwrap_or(""),
            "manifest_sha256 = \"0000000000000000000000000000000000000000000000000000000000000000\"",
        );
        fs::write(&manifest_path, corrupted).unwrap();
        assert_eq!(
            verify_manifest_integrity(&root.join("plug")),
            Err(403),
            "tampered manifest_sha256 must be rejected"
        );
    }

    #[cfg(not(debug_assertions))]
    #[test]
    fn per_file_verify_and_load_valid() {
        use crate::integrity::write_per_file_manifest;
        let root = mktemp_root("perfile_load_valid");
        mk_plugin(&root, "plug", "index.html", "<h1>hello</h1>"); // creates ui/index.html
        write_per_file_manifest(&root, "plug").unwrap();
        let bytes =
            crate::integrity::verify_and_load_plugin_asset("plug", &root, "ui/index.html").unwrap();
        assert_eq!(bytes, b"<h1>hello</h1>", "must return the correct bytes");
    }

    #[cfg(not(debug_assertions))]
    #[test]
    fn per_file_verify_and_load_modified_file_rejected() {
        use crate::integrity::write_per_file_manifest;
        let root = mktemp_root("perfile_load_modified");
        mk_plugin(&root, "plug", "index.html", "original"); // creates ui/index.html
        write_per_file_manifest(&root, "plug").unwrap();
        // Modify file after manifest was written
        fs::write(root.join("plug/ui/index.html"), "MODIFIED").unwrap();
        let result = crate::integrity::verify_and_load_plugin_asset("plug", &root, "ui/index.html");
        assert_eq!(result, Err(403), "modified file must be rejected with 403");
    }

    #[cfg(not(debug_assertions))]
    #[test]
    fn per_file_verify_and_load_file_not_in_manifest_is_404() {
        use crate::integrity::write_per_file_manifest;
        let root = mktemp_root("perfile_not_in_manifest");
        mk_plugin(&root, "plug", "index.html", "hello"); // creates ui/index.html
        write_per_file_manifest(&root, "plug").unwrap();
        // Request a file that is not in the manifest
        let result =
            crate::integrity::verify_and_load_plugin_asset("plug", &root, "ui/missing.html");
        assert_eq!(result, Err(404), "file not in manifest must be 404");
    }

    #[cfg(not(debug_assertions))]
    #[test]
    fn per_file_legacy_format_still_works() {
        // manifest with [integrity].sha256 (old directory-hash format) must still work
        let root = mktemp_root("perfile_legacy_compat");
        mk_plugin_with_manifest(&root, "plug", "placeholder");
        let actual = compute_plugin_hash(&root.join("plug")).unwrap();
        mk_plugin_with_manifest(&root, "plug", &actual);
        let result = crate::integrity::verify_and_load_plugin_asset("plug", &root, "ui/index.html");
        assert!(
            result.is_ok(),
            "legacy directory-hash format must still work: {:?}",
            result
        );
        assert_eq!(result.unwrap(), b"hello");
    }

    #[cfg(not(debug_assertions))]
    #[test]
    fn per_file_toctou_race_zero_bypass() {
        use crate::integrity::write_per_file_manifest;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let root = mktemp_root("perfile_toctou_race");
        mk_plugin(&root, "plug", "index.html", "benign_content"); // creates ui/index.html
        write_per_file_manifest(&root, "plug").unwrap();

        // Baseline verify
        let baseline =
            crate::integrity::verify_and_load_plugin_asset("plug", &root, "ui/index.html").unwrap();
        assert_eq!(baseline, b"benign_content");

        // Attacker thread: alternate between benign and MAL
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = stop.clone();
        let target = root.join("plug/ui/index.html");
        let attacker = std::thread::spawn(move || {
            let mut toggle = false;
            while !stop_flag.load(Ordering::Relaxed) {
                let content: &[u8] = if toggle { b"benign_content" } else { b"MAL" };
                let _ = fs::write(&target, content);
                toggle = !toggle;
                std::thread::yield_now();
            }
        });

        let mut ok_benign = 0u32;
        let mut ok_bypass = 0u32;
        let mut err_403 = 0u32;
        let mut err_other = 0u32;
        for _ in 0..500 {
            match crate::integrity::verify_and_load_plugin_asset("plug", &root, "ui/index.html") {
                Ok(b) if b == b"benign_content" => ok_benign += 1,
                Ok(_) => ok_bypass += 1,
                Err(403) => err_403 += 1,
                Err(_) => err_other += 1,
            }
        }

        stop.store(true, Ordering::Relaxed);
        let _ = attacker.join();

        eprintln!("per-file B5 stats: ok_benign={ok_benign} ok_bypass={ok_bypass} err_403={err_403} err_other={err_other}");
        assert_eq!(ok_bypass, 0, "B5 per-file BYPASS: {ok_bypass} serves returned bytes different from what the hash verified");
        assert!(
            err_403 + ok_benign > 0,
            "test did not exercise any real request"
        );
    }

    // ─────────────────────────────────────────────────────────────────
    // WSA-004 / WSD-003 / WSE-002 — open_external_url host+scheme allowlist.
    // The old starts_with("http") gate let ANY origin through
    // (phishing/drive-by). is_allowed_external_url parses scheme AND host.
    // ─────────────────────────────────────────────────────────────────
    #[test]
    fn external_url_allows_allowlisted_hosts_and_subdomains() {
        assert!(is_allowed_external_url("https://server-nexe.com"));
        assert!(is_allowed_external_url("http://server-nexe.com"));
        assert!(is_allowed_external_url(
            "https://server-nexe.com/trajectoria"
        ));
        assert!(is_allowed_external_url("https://docs.server-nexe.com/x"));
        assert!(is_allowed_external_url(
            "https://huggingface.co/settings/tokens"
        ));
        // host comparison is case-insensitive
        assert!(is_allowed_external_url("https://SERVER-NEXE.COM/x"));
    }

    #[test]
    fn external_url_rejects_foreign_hosts_and_bad_schemes() {
        // Foreign origin — the core phishing vector the prefix check allowed.
        assert!(!is_allowed_external_url("https://evil.example/phish"));
        // Suffix / lookalike tricks must NOT match the allowlist.
        assert!(!is_allowed_external_url(
            "https://server-nexe.com.evil.com/x"
        ));
        assert!(!is_allowed_external_url("https://evilserver-nexe.com/x"));
        assert!(!is_allowed_external_url("https://notserver-nexe.com"));
        // Non-http(s) schemes.
        assert!(!is_allowed_external_url("javascript:alert(1)"));
        assert!(!is_allowed_external_url("file:///etc/passwd"));
        assert!(!is_allowed_external_url("ftp://server-nexe.com/x"));
        // Unparseable / hostless.
        assert!(!is_allowed_external_url("server-nexe.com"));
        assert!(!is_allowed_external_url(""));
        assert!(!is_allowed_external_url("https://"));
    }
}

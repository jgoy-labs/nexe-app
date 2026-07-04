//! Sidecar state types, path resolvers, and bundle extraction.
//!
//! 2026-05-08: refactor to reduce the size of `lib.rs`.
//! 2026-05-12: tarball extraction (sidecar-bundle.tar.gz).
//!
//! Contains:
//! - Tauri sidecar state types ([`SidecarPort`], [`HttpClient`], [`SidecarChild`])
//! - Dev/prod path resolvers ([`resolve_sidecar_path_dev`], [`resolve_sidecar_path_prod`])
//! - Ephemeral port reservation + restart concurrency guard
//!
//! The bundle extraction + SHA-256 integrity helpers
//! (`ensure_sidecar_extracted` and friends) live in [`crate::sidecar_extract`]
//! (split 2026-05-30 to keep this module under the NLOC budget).
//!
//! The queue infra (PENDING_COUNT, PendingGuard, try_acquire_pending_slot) lives
//! in [`crate::handler`] because it is specific to the plugin:// handler pool.

use std::net::TcpListener;
use std::path::PathBuf;
use std::process::Child;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::Mutex;

// ─── Sidecar state types ──────────────────────────────────────────────────────

/// Sidecar port exposed as Tauri state (2026-05-02).
///
/// `fetch_from_sidecar` reads it to validate URLs with strict `expected_port`.
///
/// Wraps an `AtomicU16` so it can be reassigned at
/// runtime when `restart_sidecar` spawns a fresh process on a new ephemeral
/// port. Lock-free reads (single CPU instruction) keep the hot
/// `fetch_from_sidecar` path identical to the previous immutable struct.
///
/// Rationale: AtomicU16 beats RwLock here because
/// reads are massive (every webview invoke), writes are rare (only restart),
/// and a u16 has no half-state invariant so a primitive atomic is enough.
pub struct SidecarPort(pub AtomicU16);

impl SidecarPort {
    /// Build a new port state wrapping the given initial value.
    pub fn new(port: u16) -> Self {
        Self(AtomicU16::new(port))
    }

    /// Read the current sidecar port. Lock-free (one CPU `load`).
    pub fn get(&self) -> u16 {
        self.0.load(Ordering::Acquire)
    }

    /// Replace the sidecar port with a fresh value (called by `restart_sidecar`).
    pub fn set(&self, port: u16) {
        self.0.store(port, Ordering::Release);
    }
}

/// Values resolved once at `setup_services` that `restart_sidecar`
/// needs to spawn a fresh sidecar process. The auth token, api key and HTTP
/// client are already in Tauri state under their own types and are looked up
/// from there at restart time; we only persist what would otherwise be lost
/// (paths computed from `app.handle()` at setup time).
pub struct SpawnContext {
    pub sidecar_path: PathBuf,
    pub sidecar_data_dir: Option<PathBuf>,
    pub stdout_log_path: Option<PathBuf>,
}

// ─── Restart concurrency guard ────────────────────────────────────────────────

/// Global flag that prevents two `restart_sidecar` invocations from
/// racing (e.g. the wizard fires the command twice from a double-click, or the
/// frontend mistakenly re-triggers it). Modeled after `DIALOG_SHOWING` in
/// `lifecycle.rs`: first caller wins, others get an immediate `Err`.
pub(crate) static RESTART_IN_PROGRESS: AtomicBool = AtomicBool::new(false);

/// Attempt to acquire the restart-in-progress flag.
///
/// Returns `true` if the caller won (was `false`, now `true`) and may proceed;
/// `false` if another caller is already restarting.
///
/// Atomic `swap(true, AcqRel)` — same pattern as `graceful_quit_try_acquire`.
pub(crate) fn restart_try_acquire() -> bool {
    !RESTART_IN_PROGRESS.swap(true, Ordering::AcqRel)
}

/// RAII guard that releases `RESTART_IN_PROGRESS` on drop, including panics.
/// Construct one immediately after a successful `restart_try_acquire()`.
pub(crate) struct RestartGuard;

impl Drop for RestartGuard {
    fn drop(&mut self) {
        RESTART_IN_PROGRESS.store(false, Ordering::Release);
    }
}

/// Reserve an ephemeral port on 127.0.0.1 and return its number.
///
/// Binds `127.0.0.1:0` (OS assigns a free port), reads the assigned port,
/// then drops the listener so the sidecar can bind to the same port.
/// The TOCTOU window between drop and sidecar bind is microscopic (µs on
/// loopback) and acceptable for a local-only sidecar.
///
/// N2 (server-nexe contract): Tauri is responsible for port management.
/// server-nexe runs with NEXE_SIDECAR=1 and must NOT kill processes on port
/// conflict — it exits with error and lets Tauri retry. Use `verify_port_free`
/// right before spawn to detect the rare TOCTOU race before it reaches server-nexe.
pub fn reserve_ephemeral_port() -> Result<u16, String> {
    let listener =
        TcpListener::bind("127.0.0.1:0").map_err(|e| format!("reserve_ephemeral_port: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("local_addr: {e}"))?
        .port();
    Ok(port)
    // listener dropped here — port released for sidecar to bind
}

/// N2: verify the port is still free right before spawn (closes the TOCTOU window).
///
/// Attempts a fast TCP connect with a 50ms timeout. A refused connection means
/// the port is free (expected). A successful connection means another process
/// grabbed the port in the gap between `reserve_ephemeral_port` and this call.
///
/// Returns `Ok(())` if free, `Err(...)` if in use.
pub fn verify_port_free(port: u16) -> Result<(), String> {
    use std::net::{SocketAddr, TcpStream};
    use std::time::Duration;
    let addr: SocketAddr = format!("127.0.0.1:{port}")
        .parse()
        .map_err(|e| format!("verify_port_free parse: {e}"))?;
    match TcpStream::connect_timeout(&addr, Duration::from_millis(50)) {
        Err(_) => Ok(()), // connection refused = port is free
        Ok(_) => Err(format!(
            "port {port} is already in use (TOCTOU race or leftover process)"
        )),
    }
}

/// Reusable `reqwest::Client` across `fetch_from_sidecar` calls (2026-05-02).
///
/// **Bug fix:** an earlier revision created a new `Client` on every invoke
/// (HTTP pool + DNS + TLS session cache re-initialized each time).
/// 100 rapid clicks = 100 pools = ~300-500 fds → EMFILE risk.
/// Solution: register the client in Tauri state at `setup()` and clone
/// an Arc-handle on each invoke (`Client` internally is `Arc<ClientRef>`, cheap to clone).
pub struct HttpClient(pub reqwest::Client);

/// Handle to the sidecar process spawned at setup() (2026-05-02).
///
/// `Mutex<Option<Child>>` because:
/// - `Mutex` allows exclusive take() for the kill in lifecycle (prevents double kill).
/// - `Option<Child>` because take() leaves `None` after kill, an idempotent
///   way to know if the process has already been handled.
///
/// The `graceful_quit` lifecycle (Phase 2) will:
/// 1. POST /admin/system/shutdown with Bearer token (5s timeout via reqwest)
/// 2. On timeout or error → `child.kill()` forces it (SIGKILL)
/// 3. `child.wait()` to avoid zombie
pub struct SidecarChild(pub Mutex<Option<Child>>);

/// Path to the file capturing the Python sidecar's stdout+stderr.
///
/// The Python sidecar writes its own logs through the internal logger
/// (`NEXE_LOGS_DIR`). But if it crashes before the logger is initialized
/// (import error, `.so` blocked by Gatekeeper, segfault at the first
/// instant), nothing is written to disk. We capture stdout/stderr to
/// `<sidecar_data_dir>/logs/sidecar-stdout.log` to expose these
/// pre-logger crashes.
///
/// Registered as Tauri state for the tray menu ("Open sidecar log").
/// May be absent in dev mode (stdout inherits from the parent terminal).
pub struct SidecarLogPath(pub PathBuf);

// ─── Path resolvers ───────────────────────────────────────────────────────────

/// Resolves the absolute path to the `nexe-sidecar` launcher depending on the
/// mode (dev vs prod). Original implementation 2026-05-02.
///
/// **Dev mode** (`cfg!(debug_assertions)`, run with `pnpm tauri dev`):
///   `<project-root>/target/sidecar/nexe-sidecar` — generated by
///   `scripts/build-sidecar.sh`. The PBS venv lives alongside (`target/sidecar/venv/`)
///   and the .sh launcher resolves it via `dirname $0`.
///
/// **Prod mode** (bundled .app):
///   `<bundle resources>/binaries/nexe-sidecar-<host-triple>` — copied by Tauri
///   externalBin during `pnpm tauri build`. Note: run `pnpm tauri:build` (not
///   `tauri build` directly) so that `scripts/pre-bundle-sidecar.sh` copies
///   the venv into the bundle before Tauri packages it (2026-05-12 tarball fix).
pub(crate) fn resolve_sidecar_path(_app: &tauri::AppHandle) -> Result<PathBuf, String> {
    if cfg!(debug_assertions) {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        resolve_sidecar_path_dev(&manifest_dir)
    } else {
        let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
        resolve_sidecar_path_prod(&exe)
    }
}

/// Helper extracted from `resolve_sidecar_path`. Takes `manifest_dir` to
/// allow tests with temporary directories (without depending on the real `CARGO_MANIFEST_DIR`).
/// Returns the path to the dev `nexe-sidecar` or an error if it does not exist.
pub(crate) fn resolve_sidecar_path_dev(manifest_dir: &std::path::Path) -> Result<PathBuf, String> {
    let project_root = manifest_dir.parent().ok_or("manifest_dir has no parent")?;
    let path = project_root
        .join("target")
        .join("sidecar")
        .join("nexe-sidecar");
    if !path.is_file() {
        return Err(format!(
            "sidecar dev path does not exist: {} — run scripts/build-sidecar.sh",
            path.display()
        ));
    }
    Ok(path)
}

/// Helper extracted from `resolve_sidecar_path`. Takes `exe_path` to
/// allow tests without calling `current_exe()`. Tauri externalBin copies
/// the launcher to the main binary directory (`Contents/MacOS/`), stripping
/// the host triple suffix (e.g. `nexe-sidecar-aarch64-apple-darwin` → `nexe-sidecar`).
///
/// 2026-05-12 tarball bundle: venv+app are bundled as `sidecar-bundle.tar.gz` (single resource)
/// and extracted lazily to `app_data_dir/sidecar/` by `ensure_sidecar_extracted`.
/// The launcher finds venv/app via `NEXE_SIDECAR_DIR` (set by Rust spawner in release mode).
pub(crate) fn resolve_sidecar_path_prod(exe_path: &std::path::Path) -> Result<PathBuf, String> {
    let dir = exe_path.parent().ok_or("exe has no parent")?;
    // On Windows the externalBin keeps its .exe suffix (Tauri strips the host triple
    // but not the extension). This stub is INERT — the runtime spawns
    // venv\Scripts\python.exe from the extracted data-dir (build_windows_sidecar_command);
    // this path only needs to exist to satisfy the gate below.
    #[cfg(windows)]
    let path = dir.join("nexe-sidecar.exe");
    #[cfg(not(windows))]
    let path = dir.join("nexe-sidecar");
    if !path.is_file() {
        return Err(format!(
            "sidecar prod path does not exist: {} — run scripts/build-sidecar.sh + pnpm tauri:build",
            path.display()
        ));
    }
    Ok(path)
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    fn mktemp_root(test_name: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "nexe-sidecar-test-{}-{}",
            test_name,
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        base
    }

    // Tests for resolve_sidecar_path_dev / _prod (cross-validation)

    #[test]
    fn resolve_sidecar_dev_returns_target_sidecar_when_present() {
        let root = mktemp_root("sidecar-dev-ok");
        let manifest_dir = root.join("src-tauri");
        let target_sidecar = root.join("target").join("sidecar");
        fs::create_dir_all(&manifest_dir).unwrap();
        fs::create_dir_all(&target_sidecar).unwrap();
        let launcher = target_sidecar.join("nexe-sidecar");
        fs::write(&launcher, "#!/bin/bash\nexit 0\n").unwrap();

        let path = resolve_sidecar_path_dev(&manifest_dir).expect("should resolve");
        assert_eq!(path, launcher);
    }

    #[test]
    fn resolve_sidecar_dev_errors_when_launcher_missing() {
        let root = mktemp_root("sidecar-dev-missing");
        let manifest_dir = root.join("src-tauri");
        fs::create_dir_all(&manifest_dir).unwrap();

        let err = resolve_sidecar_path_dev(&manifest_dir).expect_err("should fail");
        assert!(
            err.contains("does not exist"),
            "error should mention path missing: got {err:?}"
        );
        assert!(
            err.contains("build-sidecar.sh"),
            "error should hint at build-sidecar.sh: got {err:?}"
        );
    }

    #[test]
    fn resolve_sidecar_dev_errors_when_no_parent() {
        let err = resolve_sidecar_path_dev(Path::new("/")).expect_err("should fail");
        assert!(err.contains("has no parent"), "got {err:?}");
    }

    #[test]
    fn resolve_sidecar_prod_returns_sibling_when_present() {
        let root = mktemp_root("sidecar-prod-ok");
        let macos_dir = root.join("Contents").join("MacOS");
        fs::create_dir_all(&macos_dir).unwrap();
        let exe = macos_dir.join("nexe-app");
        fs::write(&exe, b"binary").unwrap();
        #[cfg(windows)]
        let launcher = macos_dir.join("nexe-sidecar.exe");
        #[cfg(not(windows))]
        let launcher = macos_dir.join("nexe-sidecar");
        fs::write(&launcher, "#!/bin/bash\nexit 0\n").unwrap();

        let path = resolve_sidecar_path_prod(&exe).expect("should resolve");
        assert_eq!(path, launcher);
    }

    #[test]
    fn resolve_sidecar_prod_errors_when_launcher_missing() {
        let root = mktemp_root("sidecar-prod-missing");
        let macos_dir = root.join("Contents").join("MacOS");
        fs::create_dir_all(&macos_dir).unwrap();
        let exe = macos_dir.join("nexe-app");
        fs::write(&exe, b"binary").unwrap();

        let err = resolve_sidecar_path_prod(&exe).expect_err("should fail");
        assert!(err.contains("does not exist"), "got {err:?}");
        assert!(
            err.contains("build-sidecar.sh"),
            "should hint at build step: got {err:?}"
        );
    }

    // Test kill_sidecar_child with real subprocess (~60s sleeper)

    #[test]
    fn kill_sidecar_child_kills_running_process() {
        // Windows has no `sleep`; ping -n 60 is the stdin-free equivalent
        // (timeout.exe refuses redirected input, e.g. under SSH/CI).
        #[cfg(windows)]
        let child = std::process::Command::new("cmd")
            .args(["/c", "ping -n 60 127.0.0.1 >nul"])
            .spawn()
            .expect("spawn ping sleeper should succeed on Windows");
        #[cfg(not(windows))]
        let child = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .expect("spawn sleep should succeed on macOS/Linux");
        let pid = child.id();
        let mutex = Mutex::new(Some(child));

        let returned = crate::lifecycle::kill_sidecar_child(&mutex);
        assert_eq!(returned, Some(pid), "should return the killed pid");

        let returned2 = crate::lifecycle::kill_sidecar_child(&mutex);
        assert_eq!(returned2, None, "second call returns None (idempotent)");
    }

    #[test]
    fn kill_sidecar_child_idempotent_on_empty_mutex() {
        let mutex: Mutex<Option<std::process::Child>> = Mutex::new(None);
        let returned = crate::lifecycle::kill_sidecar_child(&mutex);
        assert_eq!(returned, None);
    }

    // ─── SidecarPort AtomicU16 + restart guard ──────────────────────

    /// `SidecarPort::get` must return the value passed to `new` before any `set`.
    #[test]
    fn sidecar_port_get_returns_initial_value() {
        let port = SidecarPort::new(54321);
        assert_eq!(port.get(), 54321);
    }

    /// `SidecarPort::set` followed by `get` must observe the new value (Acquire/Release).
    #[test]
    fn sidecar_port_set_updates_visible_via_get() {
        let port = SidecarPort::new(54321);
        port.set(54322);
        assert_eq!(port.get(), 54322);
    }

    /// `restart_try_acquire` returns true on the first call, false while the
    /// flag is held. Mutation testing: if `swap(true, AcqRel)` were replaced by
    /// `store(true, ...)` the second caller would still see `false→true` (and
    /// erroneously win), so this test would catch it.
    #[test]
    fn restart_try_acquire_first_caller_wins() {
        RESTART_IN_PROGRESS.store(false, Ordering::SeqCst);
        assert!(restart_try_acquire());
        assert!(!restart_try_acquire());
        // Cleanup
        RESTART_IN_PROGRESS.store(false, Ordering::SeqCst);
    }

    /// `RestartGuard` must release `RESTART_IN_PROGRESS` on drop — including the
    /// normal scope-exit case and panic unwinding (RAII semantics).
    #[test]
    fn restart_guard_releases_flag_on_drop() {
        RESTART_IN_PROGRESS.store(false, Ordering::SeqCst);
        assert!(restart_try_acquire());
        {
            let _g = RestartGuard;
            assert!(RESTART_IN_PROGRESS.load(Ordering::SeqCst));
        }
        assert!(!RESTART_IN_PROGRESS.load(Ordering::SeqCst));
    }

    /// Concurrent attempts to acquire the restart flag: exactly one wins.
    /// Same shape as `try_acquire_concurrent_only_one_wins` in `lifecycle.rs`
    /// for the dialog guard.
    #[test]
    fn restart_try_acquire_concurrent_only_one_wins() {
        use std::sync::Arc;
        RESTART_IN_PROGRESS.store(false, Ordering::SeqCst);
        let winners = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let barrier = Arc::new(std::sync::Barrier::new(10));
        let handles: Vec<_> = (0..10)
            .map(|_| {
                let w = winners.clone();
                let b = barrier.clone();
                std::thread::spawn(move || {
                    b.wait();
                    if restart_try_acquire() {
                        w.fetch_add(1, Ordering::Relaxed);
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(
            winners.load(Ordering::Relaxed),
            1,
            "exactly one thread must acquire the restart flag"
        );
        RESTART_IN_PROGRESS.store(false, Ordering::SeqCst);
    }

    /// `reserve_ephemeral_port` must not hand the same port twice in rapid
    /// succession — the OS picks an unused port each time the listener drops.
    /// Empirical: tested 100 iterations in a tight loop.
    #[test]
    fn reserve_ephemeral_port_no_duplicates() {
        let mut ports = Vec::with_capacity(100);
        for _ in 0..100 {
            ports.push(reserve_ephemeral_port().expect("reserve must succeed"));
        }
        let mut sorted = ports.clone();
        sorted.sort_unstable();
        sorted.dedup();
        // Allow up to 5 collisions in 100 iterations — OS may reuse a recently
        // released ephemeral port. The test fails on systemic reuse (many
        // collisions), which would indicate the listener is not actually
        // dropping in time or `bind 0` is broken.
        let unique = sorted.len();
        assert!(
            unique >= 95,
            "expected ≥95 unique ports out of 100, got {unique}"
        );
    }
}

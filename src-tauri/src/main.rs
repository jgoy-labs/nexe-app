// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::env;
use std::fs;
#[cfg(unix)]
use std::fs::OpenOptions;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// Bundle id — the SAME identifier the uninstall sweep is keyed on
/// (`onboarding_cmd::library_data_dirs`, `logging.rs`).
const BUNDLE_ID: &str = "com.nexe.app";

/// Where crash reports live, under `base` = the platform's local data dir.
///
/// Finding 838: this used to be `<base>/nexe-app/crashes` — the bare product
/// name, NOT the bundle id — so the directory sat outside every uninstall
/// sweep and 0600 stack traces survived a "remove everything" forever. Keyed on
/// the bundle id, the reports fall inside a directory the wipe already removes
/// on every platform:
///   - macOS:   `~/Library/Application Support/com.nexe.app` == `app_data_dir()`;
///   - Linux:   `~/.local/share/com.nexe.app` (a `library_data_dirs` entry);
///   - Windows: `%LOCALAPPDATA%\com.nexe.app` (a `library_data_dirs` entry).
///
/// Deliberately NOT added to `library_data_dirs` (that would break its
/// bundle-id invariant and the ubuntu-22.04 CI assert): the sweep already
/// covers the parent, the fix belongs on the writer's side.
fn crash_dir_under(base: PathBuf) -> PathBuf {
    base.join(BUNDLE_ID).join("crashes")
}

fn main() {
    // Minimal panic hook.
    //
    // Crash reports written to `dirs::data_local_dir()/com.nexe.app/crashes/`
    // (not `/tmp` world-readable). Mode 0600 on Unix to prevent exfiltration
    // of stack traces by other users on the machine. Windows: `%LOCALAPPDATA%`.
    //
    // Backtrace truncated to 10 KB — if a recursive panic generates a trace
    // of megabytes, filling the app-data directory disk is not acceptable.
    //
    // Required because panic=abort shows nothing on Windows (windows_subsystem="windows").
    // The hook is called even with panic=abort (Rust guarantee).
    std::panic::set_hook(Box::new(|info| {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let pid = std::process::id();

        // Use the bundle-id data dir (covered by the uninstall sweep), not /tmp.
        let crash_dir = {
            #[cfg(unix)]
            {
                crash_dir_under(dirs::data_local_dir().unwrap_or_else(env::temp_dir))
            }
            #[cfg(windows)]
            {
                crash_dir_under(
                    env::var("LOCALAPPDATA")
                        .map(PathBuf::from)
                        .unwrap_or_else(|_| env::temp_dir()),
                )
            }
        };
        let _ = fs::create_dir_all(&crash_dir);
        let crash_path = crash_dir.join(format!("crash-{ts}-{pid}.txt"));

        let msg_raw = info
            .payload()
            .downcast_ref::<&str>()
            .copied()
            .map(|s| s.to_string())
            .or_else(|| {
                info.payload()
                    .downcast_ref::<String>()
                    .map(|s| s.to_string())
            })
            .unwrap_or_else(|| "<non-str panic>".to_string());

        // Sanitize control chars from the panic message before writing to
        // crash file or stderr. A panic message that includes user-controlled input
        // (e.g. `format!("bad input: {}", evil)`) could contain ANSI escape sequences
        // (\x1b[...m) that would corrupt log monitors or terminals that render the
        // crash file. Allow '\n' for readability; strip everything else in 0x00-0x1f.
        // Also cap at 1024 chars — panic messages should not be unbounded.
        let msg: String = msg_raw
            .chars()
            .filter(|c| !c.is_control() || *c == '\n')
            .take(1024)
            .collect();

        // Note on panic = "abort" + RAII Drop semantics.
        // `[profile.release] panic = "abort"` means that after this hook returns,
        // the process calls abort() immediately — RAII Drop is NOT guaranteed to run.
        // Impact on guards (DepthGuard, PendingGuard, MutexGuard):
        //   - AtomicUsize HANDLER_DEPTH and PENDING_COUNT may not decrement.
        //   - MutexGuards will not unlock (poison flag is set on lock-held panics).
        // These guards are designed for per-request lifetime on worker threads.
        // A panicking worker thread dies, the threadpool spawns a replacement, and the
        // counters reset as requests drain. For a single-threaded panic this means
        // the process aborts anyway. Document here so future reviewers do not add
        // cleanup logic inside Drop that relies on running post-abort.
        let backtrace = std::backtrace::Backtrace::capture().to_string();

        // Truncate backtrace to 10 KB for DoS prevention.
        let backtrace_truncated: String = backtrace.chars().take(10_000).collect();
        let content = format!("nexe-app crash\n\n{msg}\n\nBacktrace:\n{backtrace_truncated}");

        // Mode 0600 on Unix (read/write owner only). Windows has no ACL
        // through OpenOptions → fallback to fs::write (inherits dir perm,
        // which lives under %LOCALAPPDATA% protected by user profile).
        #[cfg(unix)]
        {
            let write_res = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&crash_path);
            if let Ok(mut f) = write_res {
                let _ = f.write_all(content.as_bytes());
            }
        }
        #[cfg(windows)]
        {
            let _ = fs::write(&crash_path, &content);
        }

        let _ = std::io::stderr().write_all(
            format!(
                "PANIC: {msg}\n(crash report saved to {})\n",
                crash_path.display()
            )
            .as_bytes(),
        );
    }));

    nexe_app_lib::run()
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    #[test]
    fn crash_reports_land_inside_the_wiped_bundle_dir() {
        // 838: the crash dir MUST be keyed on the bundle id. `<base>/nexe-app`
        // (the pre-fix name) is outside every uninstall sweep.
        let dir = super::crash_dir_under(PathBuf::from("/base"));
        assert_eq!(dir, Path::new("/base/com.nexe.app/crashes"));
        assert!(
            dir.starts_with(Path::new("/base").join(super::BUNDLE_ID)),
            "must be contained in the bundle dir the wipe removes: {}",
            dir.display()
        );
        assert!(
            !dir.components()
                .any(|c| c.as_os_str() == std::ffi::OsStr::new("nexe-app")),
            "the bare product name is the pre-838 path: {}",
            dir.display()
        );
    }

    #[test]
    fn crash_dir_is_relative_to_the_given_base_only() {
        // No hidden `$HOME` lookups: the caller injects the base, so the test
        // never touches (nor depends on) the real home.
        let tmp = PathBuf::from("/tmp/x y");
        assert_eq!(
            super::crash_dir_under(tmp.clone()),
            tmp.join("com.nexe.app").join("crashes")
        );
    }
}

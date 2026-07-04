//! K-002: Windows Job Object — kill-on-close containment for the sidecar tree.
//!
//! `#[cfg(unix)] cmd.process_group(0)` has no Windows equivalent, so before
//! this module a Tauri crash (any death that skips the lifecycle kill path)
//! left the sidecar python.exe — and its grandchildren (Ollama, model
//! runners) — orphaned, holding RAM, the port and the Qdrant lock (observed
//! live on the Win11 ARM64 VM, 2026-06-11).
//!
//! Mechanism: one anonymous Job Object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`
//! is created lazily and held for the whole life of the Tauri process. Every
//! sidecar child is assigned to it right after spawn. The handle is never
//! closed on purpose: when the parent dies — clean exit or crash — the OS
//! closes the handle, the job closes, and the kernel terminates the entire
//! tree. Children inherit job membership, so grandchildren are covered
//! (unless they breakaway explicitly, which ours never request).
//!
//! The `taskkill /T /F` in `lifecycle.rs` stays as a best-effort fallback for
//! the case where job creation/assignment failed (logged below).

use std::os::windows::io::AsRawHandle;
use std::sync::OnceLock;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};

/// Owned Job Object handle, shareable across threads.
struct JobHandle(HANDLE);

// SAFETY: a HANDLE is an opaque kernel identifier, not a pointer we
// dereference; the Job Object APIs are thread-safe and the handle is never
// mutated after creation.
unsafe impl Send for JobHandle {}
unsafe impl Sync for JobHandle {}

/// Process-wide job. `Option` caches a creation failure too: if the kernel
/// refuses the job once (policy, exotic sandboxing) it will refuse it again,
/// and the taskkill fallback in lifecycle.rs still applies.
static SIDECAR_JOB: OnceLock<Option<JobHandle>> = OnceLock::new();

fn last_os_error() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

fn create_kill_on_close_job() -> Option<JobHandle> {
    // SAFETY: null attributes + null name = anonymous job; NULL return on failure.
    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job.is_null() {
        tracing::warn!(err = last_os_error(), "CreateJobObjectW failed");
        return None;
    }
    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    // SAFETY: `info` is a fully initialised POD and the size matches the type.
    let ok = unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if ok == 0 {
        tracing::warn!(err = last_os_error(), "SetInformationJobObject failed");
        // SAFETY: `job` is a valid handle we own.
        unsafe { CloseHandle(job) };
        return None;
    }
    Some(JobHandle(job))
}

/// Assign `child` to the process-wide KILL_ON_JOB_CLOSE job.
///
/// Returns `false` when the job could not be created or the assignment
/// failed — callers should log and rely on the `taskkill /T` fallback.
pub fn assign_to_sidecar_job(child: &std::process::Child) -> bool {
    let Some(job) = SIDECAR_JOB.get_or_init(create_kill_on_close_job) else {
        return false;
    };
    // SAFETY: `job.0` is a valid job handle we own; the child handle is
    // borrowed from a live `Child`; AssignProcessToJobObject is thread-safe.
    let ok = unsafe { AssignProcessToJobObject(job.0, child.as_raw_handle() as HANDLE) };
    if ok == 0 {
        tracing::warn!(
            pid = child.id(),
            err = last_os_error(),
            "AssignProcessToJobObject failed"
        );
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};
    use std::time::Duration;

    fn spawn_long_child() -> std::process::Child {
        // ping -n 60 = ~60s sleep without needing console stdin (timeout.exe
        // refuses to run with redirected input, e.g. under SSH/CI).
        Command::new("cmd")
            .args(["/c", "ping -n 60 127.0.0.1 >nul"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn long-lived child")
    }

    /// The K-002 core property: closing the job handle kills the child.
    #[test]
    fn job_close_kills_assigned_child() {
        let job = create_kill_on_close_job().expect("job creation should succeed");
        let mut child = spawn_long_child();
        let ok = unsafe { AssignProcessToJobObject(job.0, child.as_raw_handle() as HANDLE) };
        assert_ne!(
            ok,
            0,
            "AssignProcessToJobObject failed: {}",
            last_os_error()
        );

        // Simulates the parent dying: the OS closes every handle it owned.
        unsafe { CloseHandle(job.0) };

        for _ in 0..100 {
            if child.try_wait().expect("try_wait").is_some() {
                return; // child died with the job — PASS
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        child.kill().ok();
        panic!("child survived >10s after job close — KILL_ON_JOB_CLOSE not effective");
    }

    /// The public API path: assignment succeeds and the normal kill path
    /// (child.kill) keeps working for processes inside the job.
    #[test]
    fn assign_to_sidecar_job_succeeds_and_child_still_killable() {
        let mut child = spawn_long_child();
        assert!(
            assign_to_sidecar_job(&child),
            "assignment to the global job should succeed"
        );
        child.kill().expect("kill child inside job");
        child.wait().expect("wait child");
    }
}

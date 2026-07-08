//! Global worker process group tracking for signal-safe cleanup.
//!
//! When the main process is killed (SIGINT, SIGTERM), Rust destructors
//! don't run and worker subprocesses become orphans.  This module keeps
//! a global list of worker process group IDs so a signal handler (or
//! atexit hook) can kill them all.

use std::sync::Mutex;

/// Global list of active worker process group IDs (= worker PIDs,
/// because each worker is spawned with `process_group(0)`).
static WORKER_PGIDS: Mutex<Vec<u32>> = Mutex::new(Vec::new());

/// Register a worker's PID as an active process group.
pub fn register_worker_pgid(pid: u32) {
    if let Ok(mut pgids) = WORKER_PGIDS.lock() {
        pgids.push(pid);
    }
}

/// Remove a worker's PID from the active process group list.
pub fn unregister_worker_pgid(pid: u32) {
    if let Ok(mut pgids) = WORKER_PGIDS.lock() {
        pgids.retain(|&p| p != pid);
    }
}

/// Kill all tracked worker process groups.
///
/// Called from signal handlers or atexit hooks to ensure no orphans.
/// This function is designed to be safe to call from adverse conditions
/// (lock poisoning is handled gracefully).
pub fn kill_all_worker_groups() {
    // Try to get the list; if the lock is poisoned, recover it.
    let pgids = match WORKER_PGIDS.lock() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    };

    for pid in pgids {
        kill_process_group(pid);
    }
}

/// Kill a single process group by its pgid.
#[cfg(unix)]
fn kill_process_group(pgid: u32) {
    let _ = std::process::Command::new("kill")
        .args(["-9", "--", &format!("-{pgid}")])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

#[cfg(windows)]
fn kill_process_group(pid: u32) {
    let _ = std::process::Command::new("taskkill")
        .args(["/F", "/T", "/PID", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// Install signal handlers (SIGINT, SIGTERM) that kill all worker
/// process groups before exiting.
///
/// Call this once from `main()`.  On non-Unix platforms this is a no-op.
pub fn install_cleanup_signal_handlers() {
    #[cfg(unix)]
    {
        // SAFETY: We only call signal-safe operations (kill) and _exit.
        // The kill is done via the global PGID list.  We can't use
        // std::process::exit here because it's not signal-safe, so we
        // use libc::_exit after killing children.
        unsafe {
            // SIGINT (Ctrl-C)
            let _ = libc::signal(
                libc::SIGINT,
                signal_handler as *const () as libc::sighandler_t,
            );
            // SIGTERM (kill <pid>)
            let _ = libc::signal(
                libc::SIGTERM,
                signal_handler as *const () as libc::sighandler_t,
            );
        }
    }
}

#[cfg(unix)]
#[allow(clippy::cast_possible_wrap)]
extern "C" fn signal_handler(sig: libc::c_int) {
    // Kill all worker process groups
    // We read the global directly; if the lock is poisoned we recover.
    if let Ok(pgids) = WORKER_PGIDS.lock() {
        for &pid in pgids.iter() {
            unsafe {
                let _ = libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
            }
        }
    }
    // Re-raise the signal with default handler so the process exits
    // with the correct status / signal code.
    unsafe {
        let _ = libc::signal(sig, libc::SIG_DFL);
        let _ = libc::raise(sig);
    }
}

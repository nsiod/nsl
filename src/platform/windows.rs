use std::fs;
use std::io::Write as _;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, STILL_ACTIVE};
use windows_sys::Win32::System::Threading::{
    CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, DETACHED_PROCESS, GetExitCodeProcess, OpenProcess,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE, TerminateProcess,
};

use crate::routes::RouteOwner;

/// Combined creation flags that fully detach a spawned child: no inherited
/// console, new process group (so Ctrl+C on the parent doesn't propagate),
/// and no visible console window.
const DAEMON_CREATION_FLAGS: u32 = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW;

/// RAII wrapper around a Windows process HANDLE.
struct ProcessHandle(HANDLE);

impl ProcessHandle {
    /// Open a handle with the given desired access. Returns `None` when the
    /// process does not exist or we lack permission (both treated as "not
    /// there" for our purposes).
    fn open(pid: u32, access: u32) -> Option<Self> {
        // SAFETY: OpenProcess is safe to call with any u32 pid/access; it
        // returns null on failure which we check.
        let h = unsafe { OpenProcess(access, 0, pid) };
        if h.is_null() { None } else { Some(Self(h)) }
    }
}

impl Drop for ProcessHandle {
    fn drop(&mut self) {
        // SAFETY: we hold a valid handle returned by OpenProcess.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

/// Check if a process is alive via `OpenProcess + GetExitCodeProcess`.
/// `pid == 0` is treated as "alive" (static route placeholder).
pub fn is_process_alive(pid: u32) -> bool {
    if pid == 0 {
        return true;
    }

    let Some(handle) = ProcessHandle::open(pid, PROCESS_QUERY_LIMITED_INFORMATION) else {
        return false;
    };

    let mut exit_code: u32 = 0;
    // SAFETY: handle is valid; exit_code is a live u32.
    let ok = unsafe { GetExitCodeProcess(handle.0, &mut exit_code) };
    if ok == 0 {
        return false;
    }
    exit_code == STILL_ACTIVE as u32
}

pub fn current_platform() -> &'static str {
    std::env::consts::OS
}

pub fn current_process_group(_pid: u32) -> Option<u32> {
    None
}

pub fn current_process_start_time(_pid: u32) -> Option<u64> {
    None
}

fn validate_app_owner(owner: &RouteOwner) -> anyhow::Result<()> {
    if owner.platform != current_platform() {
        anyhow::bail!(
            "route owner platform is {}, current platform is {}",
            owner.platform,
            current_platform()
        );
    }
    if !is_process_alive(owner.pid) {
        anyhow::bail!("route owner process is not alive");
    }
    Ok(())
}

pub fn kill_app_process(owner: &RouteOwner) -> anyhow::Result<()> {
    validate_app_owner(owner)?;
    terminate_process(owner.pid)
}

/// Terminate a process. Windows has no SIGTERM concept -- this is closer to
/// SIGKILL, but it matches what users expect from `nsl stop` on Windows.
pub fn terminate_process(pid: u32) -> anyhow::Result<()> {
    let Some(handle) = ProcessHandle::open(pid, PROCESS_TERMINATE) else {
        anyhow::bail!(
            "failed to stop proxy (PID {}): process not found or access denied",
            pid
        );
    };
    // SAFETY: handle is valid with PROCESS_TERMINATE access.
    let ok = unsafe { TerminateProcess(handle.0, 1) };
    if ok == 0 {
        anyhow::bail!("failed to terminate proxy (PID {})", pid);
    }
    Ok(())
}

/// Windows has no fork/setsid. The parent must spawn us with detached
/// creation flags (via `detach_spawn`), so by the time we enter this
/// function we are already a stand-alone process. We only need to write the
/// PID file and redirect stdout/stderr to the log. Console stdio was set to
/// null when the parent spawned us (`DETACHED_PROCESS`).
pub fn daemonize_self(state_dir: &Path) -> anyhow::Result<()> {
    let log_path = state_dir.join("proxy.log");
    // Open append so the parent's readiness checks don't see a truncation.
    let _log_marker = fs::File::options()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&log_path)?;

    let pid_path = state_dir.join("proxy.pid");
    let mut pid_file = fs::File::create(&pid_path)?;
    writeln!(pid_file, "{}", std::process::id())?;

    Ok(())
}

/// Apply detached-process creation flags so the spawned child survives parent
/// exit and does not inherit the parent's console.
pub fn detach_spawn(cmd: &mut std::process::Command) {
    cmd.creation_flags(DAEMON_CREATION_FLAGS);
}

/// Windows state directory root when the proxy would otherwise need root.
/// Falls back to `%LOCALAPPDATA%\nsl`, then `%TEMP%\nsl`.
pub fn privileged_state_dir() -> PathBuf {
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        return PathBuf::from(local).join("nsl");
    }
    if let Some(temp) = std::env::var_os("TEMP") {
        return PathBuf::from(temp).join("nsl");
    }
    PathBuf::from("nsl")
}

/// User home directory. Prefers `%USERPROFILE%` (Windows convention), falls
/// back to `$HOME` for MSYS/Cygwin-style environments.
pub fn user_home() -> PathBuf {
    if let Some(profile) = std::env::var_os("USERPROFILE") {
        return PathBuf::from(profile);
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home);
    }
    PathBuf::from(".")
}

/// No-op on Windows: there is no sudo, so no ownership fix-up is needed.
pub fn fix_ownership(_path: &Path) {}

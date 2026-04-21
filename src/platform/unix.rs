use std::fs;
use std::path::{Path, PathBuf};

use crate::routes::RouteOwner;

/// Check if a process is alive by sending signal 0.
///
/// `pid == 0` is treated as "alive" because it represents a static route
/// that has no owning process.
pub fn is_process_alive(pid: u32) -> bool {
    if pid == 0 {
        return true;
    }
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_ok()
}

pub fn current_platform() -> &'static str {
    std::env::consts::OS
}

pub fn current_process_group(pid: u32) -> Option<u32> {
    nix::unistd::getpgid(Some(nix::unistd::Pid::from_raw(pid as i32)))
        .ok()
        .and_then(|pgid| u32::try_from(pgid.as_raw()).ok())
}

#[cfg(target_os = "linux")]
pub fn current_process_start_time(pid: u32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = stat.rsplit_once(") ")?.1;
    after_comm.split_whitespace().nth(19)?.parse().ok()
}

#[cfg(not(target_os = "linux"))]
pub fn current_process_start_time(_pid: u32) -> Option<u64> {
    None
}

#[cfg(target_os = "linux")]
fn process_cwd(pid: u32) -> Option<String> {
    fs::read_link(format!("/proc/{pid}/cwd"))
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

#[cfg(not(target_os = "linux"))]
fn process_cwd(_pid: u32) -> Option<String> {
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
    match (owner.process_group, current_process_group(owner.pid)) {
        (Some(expected), Some(actual)) if expected == actual && actual == owner.pid => {}
        (Some(expected), Some(actual)) => {
            anyhow::bail!(
                "route owner process group changed: expected {}, got {}",
                expected,
                actual
            );
        }
        _ => anyhow::bail!("could not verify route owner process group"),
    }

    if let Some(expected) = owner.start_time {
        match current_process_start_time(owner.pid) {
            Some(actual) if actual == expected => {}
            Some(actual) => {
                anyhow::bail!(
                    "route owner start time changed: expected {}, got {}",
                    expected,
                    actual
                );
            }
            None => anyhow::bail!("could not verify route owner start time"),
        }
    }

    if let Some(cwd) = process_cwd(owner.pid)
        && cwd != owner.cwd
    {
        anyhow::bail!(
            "route owner cwd changed: expected {}, got {}",
            owner.cwd,
            cwd
        );
    }

    Ok(())
}

pub fn kill_app_process(owner: &RouteOwner) -> anyhow::Result<()> {
    validate_app_owner(owner)?;
    nix::sys::signal::killpg(
        nix::unistd::Pid::from_raw(owner.pid as i32),
        nix::sys::signal::Signal::SIGTERM,
    )
    .map_err(|e| anyhow::anyhow!("failed to terminate app process group {}: {}", owner.pid, e))
}

/// Send SIGTERM to a process. Returns `Ok(())` on success.
pub fn terminate_process(pid: u32) -> anyhow::Result<()> {
    let nix_pid = nix::unistd::Pid::from_raw(pid as i32);
    nix::sys::signal::kill(nix_pid, nix::sys::signal::Signal::SIGTERM).map_err(|e| {
        if e == nix::errno::Errno::EPERM {
            anyhow::anyhow!(
                "permission denied: cannot stop proxy (PID {}). Try: sudo nsl stop",
                pid
            )
        } else {
            anyhow::anyhow!("failed to stop proxy (PID {}): {}", pid, e)
        }
    })
}

/// Daemonize the current process (Unix: fork + setsid). After this returns
/// `Ok(())`, the caller is the daemonized child with stdout/stderr redirected
/// to `state_dir/proxy.log` and the PID written to `state_dir/proxy.pid`.
pub fn daemonize_self(state_dir: &Path) -> anyhow::Result<()> {
    let log_path = state_dir.join("proxy.log");
    let log_file = fs::File::create(&log_path)?;
    let log_err = log_file.try_clone()?;
    let pid_file = state_dir.join("proxy.pid");

    let daemonize = daemonize::Daemonize::new()
        .pid_file(&pid_file)
        .chown_pid_file(true)
        .working_directory("/")
        .stdout(log_file)
        .stderr(log_err);

    daemonize
        .start()
        .map_err(|e| anyhow::anyhow!("failed to daemonize: {}", e))
}

/// Apply platform-specific flags to a `Command` so the spawned child is
/// detached from the current process group / console.
///
/// On Unix this is a no-op: the daemonize crate (called by the child) does
/// the fork + setsid after spawn.
pub fn detach_spawn(_cmd: &mut std::process::Command) {}

/// Default state directory root for privileged proxy ports (< 1024).
pub fn privileged_state_dir() -> PathBuf {
    PathBuf::from("/tmp/nsl")
}

/// Default user home for state directory lookup.
pub fn user_home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
}

/// Fix file/directory ownership when running under sudo (Unix-only).
/// No-op on Windows.
pub fn fix_ownership(path: &Path) {
    let Some((uid, gid)) = detect_sudo_ids() else {
        return;
    };

    if let Err(err) = nix::unistd::chown(
        path,
        Some(nix::unistd::Uid::from_raw(uid)),
        Some(nix::unistd::Gid::from_raw(gid)),
    ) {
        tracing::warn!(
            "fix_ownership: chown {:?} to {}:{} failed: {}",
            path,
            uid,
            gid,
            err
        );
    }
}

pub(crate) fn detect_sudo_ids() -> Option<(nix::libc::uid_t, nix::libc::gid_t)> {
    let uid_str = std::env::var("SUDO_UID").ok()?;
    let gid_str = std::env::var("SUDO_GID").ok()?;
    let uid: nix::libc::uid_t = uid_str.parse().ok()?;
    let gid: nix::libc::gid_t = gid_str.parse().ok()?;
    Some((uid, gid))
}

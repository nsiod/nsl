mod framework;

use crate::config::Config;
use crate::discover::infer_project_name;
use crate::routes::{RouteOwner, RouteStore};
use crate::utils::{extract_hostname_prefix, format_url, format_urls, parse_hostname};
use std::sync::{
    Arc,
    atomic::{AtomicU32, Ordering},
};

#[cfg(unix)]
use framework::wait_for_app_wrapped;
use framework::{find_free_port, inject_framework_flags, replace_port_placeholders};

/// Check if nsl proxy is disabled via the NSL environment variable.
///
/// Returns `true` when `NSL` is set to `"0"` or `"skip"` (case-insensitive).
pub fn is_nsl_disabled() -> bool {
    match std::env::var("NSL") {
        Ok(val) => {
            let v = val.trim().to_lowercase();
            v == "0" || v == "skip"
        }
        Err(_) => false,
    }
}

/// Build a `tokio::process::Command` that runs the given args through the
/// platform's default shell (`sh -c` on Unix, `cmd /C` on Windows).
fn shell_command(args: &[String]) -> tokio::process::Command {
    let joined = shell_command_line(args);
    #[cfg(unix)]
    {
        let mut c = tokio::process::Command::new("sh");
        c.arg("-c").arg(joined);
        c
    }
    #[cfg(windows)]
    {
        let mut c = tokio::process::Command::new("cmd");
        c.arg("/C").arg(joined);
        c
    }
}

fn shell_command_line(args: &[String]) -> String {
    if args.len() == 1 {
        return args[0].clone();
    }

    args.iter()
        .map(|arg| shell_quote(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_process_command(args: &[String]) -> Vec<String> {
    let joined = shell_command_line(args);
    #[cfg(unix)]
    {
        vec!["sh".to_string(), "-c".to_string(), joined]
    }
    #[cfg(windows)]
    {
        vec!["cmd".to_string(), "/C".to_string(), joined]
    }
}

fn build_route_owner(pid: u32, args: &[String]) -> anyhow::Result<RouteOwner> {
    let cwd = std::env::current_dir()?.to_string_lossy().into_owned();
    Ok(RouteOwner {
        pid,
        platform: crate::platform::current_platform().to_string(),
        cwd,
        command: shell_process_command(args),
        process_group: crate::platform::current_process_group(pid),
        start_time: crate::platform::current_process_start_time(pid),
    })
}

#[cfg(unix)]
fn shell_quote(arg: &str) -> String {
    if arg.is_empty() {
        return "''".to_string();
    }
    if arg.bytes().all(|b| {
        b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'/' | b':' | b'=' | b',')
    }) {
        return arg.to_string();
    }
    format!("'{}'", arg.replace('\'', "'\\''"))
}

#[cfg(windows)]
fn shell_quote(arg: &str) -> String {
    if arg.is_empty() {
        return "\"\"".to_string();
    }
    if !arg.bytes().any(|b| {
        matches!(
            b,
            b' ' | b'\t' | b'&' | b'|' | b'<' | b'>' | b'(' | b')' | b'^' | b'"' | b'%'
        )
    }) {
        return arg.to_string();
    }
    format!("\"{}\"", arg.replace('"', "\\\""))
}

/// Spawn a command directly without proxy registration.
async fn run_direct(config: &Config, cmd: &[String]) -> anyhow::Result<()> {
    let port = config.app_port.unwrap_or(0);

    tracing::info!(
        "NSL env is set to skip; running command directly (PORT={})",
        port
    );

    let status = shell_command(cmd)
        .env("PORT", port.to_string())
        .status()
        .await?;

    if !status.success() {
        exit_with_status(status);
    }

    Ok(())
}

/// Print formatted connection info after app is ready.
fn print_connection_info(
    config: &Config,
    cmd: &[String],
    app_port: u16,
    child_pid: u32,
    hostname: &str,
    path: &str,
) {
    let prefix = extract_hostname_prefix(hostname, &config.domains);
    let urls = format_urls(
        &prefix,
        &config.domains,
        config.proxy_port,
        config.proxy_https,
        &config.domain_displays,
    );
    let version = env!("CARGO_PKG_VERSION");
    let path_suffix = if path != "/" { path } else { "" };

    println!();
    println!("nsl v{}", version);
    println!();
    println!("  App:     {}", cmd.join(" "));
    println!("  Port:    {} (allocated)", app_port);
    println!("  PID:     {}", child_pid);
    println!();
    println!("  URLs:");
    for url in &urls {
        println!("    {}{}", url, path_suffix);
    }
    println!();
    println!(
        "  Proxy:   http://127.0.0.1:{} (running)",
        config.proxy_port
    );
    println!();
    println!("  press ctrl+c to stop");
    println!();
}

/// Run an app through the nsl proxy.
pub async fn run_app(
    config: &Config,
    cmd: &[String],
    name_override: Option<&str>,
    change_origin: bool,
    path: &str,
    strip_prefix: bool,
) -> anyhow::Result<()> {
    if is_nsl_disabled() {
        return run_direct(config, cmd).await;
    }

    let hostname = if let Some(name) = name_override {
        parse_hostname(name, &config.domains).map_err(|e| anyhow::anyhow!("{}", e))?
    } else {
        let cwd = std::env::current_dir()?;
        let project_name = infer_project_name(&cwd);
        parse_hostname(&project_name, &config.domains).map_err(|e| anyhow::anyhow!("{}", e))?
    };

    // Ensure proxy is running
    if crate::proxy::ensure_proxy_running(config).await? {
        tracing::info!("proxy not running, started daemon");
    }

    // Find a port for the app
    let app_port = match config.app_port {
        Some(p) => p,
        None => find_free_port(config.app_port_range.0, config.app_port_range.1)?,
    };

    let url = format_url(
        &hostname,
        config.proxy_port,
        config.proxy_https,
        &config.domain_displays,
    );
    tracing::info!("{} -> localhost:{}", url, app_port);

    // Inject framework flags
    let final_args = replace_port_placeholders(&inject_framework_flags(cmd, app_port), app_port);

    let registered_child_pid = Arc::new(AtomicU32::new(0));
    let registered_child_pid_cb = Arc::clone(&registered_child_pid);
    let state_dir_cb = config.resolve_state_dir();
    let hostname_cb = hostname.clone();
    let path_cb = path.to_string();
    let owner_args = final_args.clone();
    let app_force = config.app_force;
    let on_child_spawned = move |child_pid: u32| {
        let owner = build_route_owner(child_pid, &owner_args)?;
        RouteStore::new(state_dir_cb)
            .add_route_with_owner(
                &hostname_cb,
                app_port,
                child_pid,
                Some(owner),
                app_force,
                change_origin,
                &path_cb,
                strip_prefix,
            )
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        registered_child_pid_cb.store(child_pid, Ordering::SeqCst);
        Ok(())
    };

    // Spawn the command with process group and await result
    let result = spawn_command(
        &final_args,
        app_port,
        &url,
        config,
        cmd,
        &hostname,
        path,
        on_child_spawned,
    )
    .await;

    // Cleanup route on exit
    let state_dir = config.resolve_state_dir();
    let store = RouteStore::new(state_dir);
    let path_filter = if path == "/" { None } else { Some(path) };
    let child_pid = registered_child_pid.load(Ordering::SeqCst);
    if child_pid != 0 {
        let _ = store.remove_route_for_pid(&hostname, path_filter, child_pid);
    }

    match result {
        Ok(0) => Ok(()),
        Ok(code) => std::process::exit(code),
        Err(e) => Err(e),
    }
}

/// Spawn a child process with signal forwarding. On Unix the child is
/// placed in its own process group so we can signal the whole tree; on
/// Windows we use a plain tokio `Child` and kill it on Ctrl+C.
#[cfg(unix)]
async fn spawn_command(
    args: &[String],
    port: u16,
    url: &str,
    config: &Config,
    original_cmd: &[String],
    hostname: &str,
    path: &str,
    on_child_spawned: impl FnOnce(u32) -> anyhow::Result<()>,
) -> anyhow::Result<i32> {
    use process_wrap::tokio::*;

    let shell_cmd = shell_command_line(args);

    let mut wrap = CommandWrap::with_new("sh", |command| {
        command
            .arg("-c")
            .arg(&shell_cmd)
            .env("PORT", port.to_string())
            .env("HOST", "127.0.0.1")
            .env("NSL_URL", url)
            .env("NSL", "1");
    });

    wrap.wrap(ProcessGroup::leader());

    let mut child = wrap.spawn()?;

    let child_pid = child.id().unwrap_or(0);

    if let Err(e) = on_child_spawned(child_pid) {
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(child_pid as i32),
            nix::sys::signal::Signal::SIGTERM,
        );
        return Err(e);
    }

    // Wait for app readiness
    if let Err(e) = wait_for_app_wrapped(port, config.ready_timeout, &mut child).await {
        tracing::error!("readiness check failed: {}", e);
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(child_pid as i32),
            nix::sys::signal::Signal::SIGTERM,
        );
        return Err(e);
    }

    // Print connection info
    print_connection_info(config, original_cmd, port, child_pid, hostname, path);

    // Set up signal forwarding and wait for child
    let pgid = nix::unistd::Pid::from_raw(child_pid as i32);
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

    let wait_fut = child.wait();

    tokio::select! {
        status = wait_fut => {
            let status = status?;
            Ok(exit_code_from_status(status).unwrap_or(0))
        }
        _ = sigint.recv() => {
            let _ = nix::sys::signal::killpg(pgid, nix::sys::signal::Signal::SIGTERM);
            let status = child.wait().await?;
            Ok(exit_code_from_status(status).unwrap_or(0))
        }
        _ = sigterm.recv() => {
            let _ = nix::sys::signal::killpg(pgid, nix::sys::signal::Signal::SIGTERM);
            let status = child.wait().await?;
            Ok(exit_code_from_status(status).unwrap_or(0))
        }
    }
}

#[cfg(windows)]
async fn spawn_command(
    args: &[String],
    port: u16,
    url: &str,
    config: &Config,
    original_cmd: &[String],
    hostname: &str,
    path: &str,
    on_child_spawned: impl FnOnce(u32) -> anyhow::Result<()>,
) -> anyhow::Result<i32> {
    use framework::wait_for_app;

    let mut cmd = shell_command(args);
    cmd.env("PORT", port.to_string())
        .env("HOST", "127.0.0.1")
        .env("NSL_URL", url)
        .env("NSL", "1");

    let mut child = cmd.spawn()?;
    let child_pid = child.id().unwrap_or(0);

    if let Err(e) = on_child_spawned(child_pid) {
        let _ = child.start_kill();
        return Err(e);
    }

    if let Err(e) = wait_for_app(port, config.ready_timeout, &mut child).await {
        tracing::error!("readiness check failed: {}", e);
        let _ = child.start_kill();
        return Err(e);
    }

    print_connection_info(config, original_cmd, port, child_pid, hostname, path);

    tokio::select! {
        status = child.wait() => {
            let status = status?;
            return Ok(exit_code_from_status(status).unwrap_or(0));
        }
        _ = tokio::signal::ctrl_c() => {
            let _ = child.start_kill();
            let status = child.wait().await?;
            return Ok(exit_code_from_status(status).unwrap_or(0));
        }
    }
}

fn exit_code_from_status(status: std::process::ExitStatus) -> Option<i32> {
    if let Some(code) = status.code() {
        if code != 0 {
            return Some(code);
        }
        None
    } else {
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            if let Some(sig) = status.signal() {
                return Some(128 + sig);
            }
        }
        Some(1)
    }
}

/// Propagate child exit status to the current process.
fn exit_with_status(status: std::process::ExitStatus) {
    if let Some(code) = exit_code_from_status(status) {
        std::process::exit(code);
    }
}

#[cfg(test)]
mod tests;

mod framework;

use crate::config::Config;
use crate::discover::infer_project_name;
use crate::routes::RouteStore;
use crate::utils::{extract_hostname_prefix, format_url, format_urls, parse_hostname};

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
    let joined = args.join(" ");
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
        propagate_exit_status(status);
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

    // Register route
    let state_dir = config.resolve_state_dir();
    let store = RouteStore::new(state_dir);
    let pid = std::process::id();
    store.add_route(
        &hostname,
        app_port,
        pid,
        config.app_force,
        change_origin,
        path,
        strip_prefix,
    )?;

    let url = format_url(
        &hostname,
        config.proxy_port,
        config.proxy_https,
        &config.domain_displays,
    );
    tracing::info!("{} -> localhost:{}", url, app_port);

    // Inject framework flags
    let final_args = replace_port_placeholders(&inject_framework_flags(cmd, app_port), app_port);

    // Spawn the command with process group and await result
    let result = spawn_command(&final_args, app_port, &url, config, cmd, &hostname, path).await;

    // Cleanup route on exit
    let state_dir = config.resolve_state_dir();
    let store = RouteStore::new(state_dir);
    let path_filter = if path == "/" { None } else { Some(path) };
    let _ = store.remove_route(&hostname, path_filter);

    result
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
) -> anyhow::Result<()> {
    use process_wrap::tokio::*;

    let shell_cmd = args.join(" ");

    let mut wrap = CommandWrap::with_new("sh", |command| {
        command
            .arg("-c")
            .arg(&shell_cmd)
            .env("PORT", port.to_string())
            .env("HOST", "127.0.0.1")
            .env("NSL_URL", url);
    });

    wrap.wrap(ProcessGroup::leader());

    let mut child = wrap.spawn()?;

    let child_pid = child.id().unwrap_or(0);

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
            propagate_exit_status(status);
        }
        _ = sigint.recv() => {
            let _ = nix::sys::signal::killpg(pgid, nix::sys::signal::Signal::SIGTERM);
            let status = child.wait().await?;
            propagate_exit_status(status);
        }
        _ = sigterm.recv() => {
            let _ = nix::sys::signal::killpg(pgid, nix::sys::signal::Signal::SIGTERM);
            let status = child.wait().await?;
            propagate_exit_status(status);
        }
    }

    Ok(())
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
) -> anyhow::Result<()> {
    use framework::wait_for_app;

    let mut cmd = shell_command(args);
    cmd.env("PORT", port.to_string())
        .env("HOST", "127.0.0.1")
        .env("NSL_URL", url);

    let mut child = cmd.spawn()?;
    let child_pid = child.id().unwrap_or(0);

    if let Err(e) = wait_for_app(port, config.ready_timeout, &mut child).await {
        tracing::error!("readiness check failed: {}", e);
        let _ = child.start_kill();
        return Err(e);
    }

    print_connection_info(config, original_cmd, port, child_pid, hostname, path);

    tokio::select! {
        status = child.wait() => {
            let status = status?;
            propagate_exit_status(status);
        }
        _ = tokio::signal::ctrl_c() => {
            let _ = child.start_kill();
            let status = child.wait().await?;
            propagate_exit_status(status);
        }
    }

    Ok(())
}

/// Propagate child exit status to the current process.
fn propagate_exit_status(status: std::process::ExitStatus) {
    if let Some(code) = status.code() {
        if code != 0 {
            std::process::exit(code);
        }
    } else {
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            if let Some(sig) = status.signal() {
                std::process::exit(128 + sig);
            }
        }
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests;

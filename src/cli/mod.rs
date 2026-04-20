mod handler;

use std::net::IpAddr;

use clap::{Parser, Subcommand};

use crate::config::Config;

/// Replace port numbers with stable, named .localhost URLs.
#[derive(Parser)]
#[command(name = "nsl", version, about)]
pub struct Cli {
    #[command(subcommand)]
    pub(super) command: Commands,
}

#[derive(Subcommand)]
pub(super) enum Commands {
    /// Infer name from project and run through proxy
    #[command(
        after_help = "Port placeholder:\n  Use NSL_PORT in child command arguments when a CLI does not read PORT.\n  Example: nsl run ./server --port NSL_PORT\n  The NSL_PORT environment variable still configures the proxy port."
    )]
    Run {
        /// Command and arguments to run
        #[arg(trailing_var_arg = true, required = true)]
        cmd: Vec<String>,

        /// Override the auto-inferred project name, optionally with a path
        /// prefix (e.g. `myapp` or `myapp:/api`)
        #[arg(short, long)]
        name: Option<String>,

        /// Use a fixed port for the app
        #[arg(short = 'p', long = "port")]
        app_port: Option<u16>,

        /// Override a route registered by another process
        #[arg(long)]
        force: bool,

        /// Rewrite Host header to target address
        #[arg(short = 'c', long)]
        change_origin: bool,

        /// Strip the path prefix (from name like `myapp:/api`) before forwarding
        #[arg(short, long)]
        strip: bool,
    },

    /// Start the proxy server
    Start {
        /// Port for the proxy (default: from config or 1355)
        #[arg(short, long)]
        port: Option<u16>,

        /// IP address to bind on (e.g. 0.0.0.0 to accept LAN traffic)
        #[arg(long, value_name = "IP")]
        bind: Option<IpAddr>,

        /// Enable HTTPS/HTTP2
        #[arg(long)]
        https: bool,

        /// Run in foreground (default: daemon mode)
        #[arg(long)]
        foreground: bool,

        /// Internal: daemonize the process before starting the proxy
        #[arg(long, hide = true)]
        daemonize: bool,
    },

    /// Stop the proxy server
    Stop,

    /// Restart the proxy server (stop then start)
    Reload {
        /// Port for the proxy (default: from config or 1355)
        #[arg(short, long)]
        port: Option<u16>,

        /// IP address to bind on (e.g. 0.0.0.0 to accept LAN traffic)
        #[arg(long, value_name = "IP")]
        bind: Option<IpAddr>,

        /// Enable HTTPS/HTTP2
        #[arg(long)]
        https: bool,

        /// Run in foreground (default: daemon mode)
        #[arg(long)]
        foreground: bool,
    },

    /// Show proxy daemon logs
    Logs {
        /// Follow log output in real time (like tail -f)
        #[arg(short, long)]
        follow: bool,

        /// Number of lines to show from the end (default: all)
        #[arg(short = 'n', long)]
        lines: Option<usize>,
    },

    /// Register a static route (e.g. for Docker)
    Route {
        /// App name, optionally with a path prefix (e.g. `myapp` or `myapp:/api`)
        name: Option<String>,

        /// Target port
        port: Option<u16>,

        /// Remove the route
        #[arg(long)]
        remove: bool,

        /// Override an existing route
        #[arg(long)]
        force: bool,

        /// Rewrite Host header to target address
        #[arg(short = 'c', long)]
        change_origin: bool,

        /// Strip the path prefix (from name like `myapp:/api`) before forwarding
        #[arg(short, long)]
        strip: bool,
    },

    /// Output the URL for a given app name (for scripts)
    Get {
        /// App name, optionally with a path prefix (e.g. `myapp` or `myapp:/api`)
        name: String,
    },

    /// Show active routes
    List,

    /// Show proxy status, active routes, and merged configuration
    Status,

    /// Add local CA to system trust store
    Trust,

    /// Manage /etc/hosts entries
    Hosts {
        #[command(subcommand)]
        action: HostsAction,
    },
}

#[derive(Subcommand)]
pub(super) enum HostsAction {
    /// Add routes to /etc/hosts
    Sync,
    /// Remove nsl entries from /etc/hosts
    Clean,
}

/// Apply CLI flag overrides to a config.
pub(super) fn apply_cli_overrides(
    config: &mut Config,
    port: Option<u16>,
    https: bool,
    bind: Option<IpAddr>,
) {
    if let Some(p) = port {
        config.proxy_port = p;
    }
    if https {
        config.proxy_https = true;
    }
    if let Some(b) = bind {
        config.proxy_bind = b;
    }
}

impl Cli {
    pub async fn run(self) -> anyhow::Result<()> {
        handler::handle(self).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn test_run_help_shows_port_placeholder() {
        let mut command = Cli::command();
        let run = command.find_subcommand_mut("run").unwrap();
        let mut help = Vec::new();

        run.write_help(&mut help).unwrap();
        let help = String::from_utf8(help).unwrap();

        assert!(help.contains("NSL_PORT"));
        assert!(help.contains("child command arguments"));
    }
}

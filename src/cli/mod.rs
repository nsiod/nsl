mod handler;

use clap::{Parser, Subcommand};

use crate::config::Config;

/// Replace port numbers with stable, named .localhost URLs.
#[derive(Parser)]
#[command(
    name = "nsl",
    version,
    about,
    after_help = "Documentation:\n  README: https://github.com/nsiod/nsl/blob/main/README.md\n  AI usage guide: https://github.com/nsiod/nsl/blob/main/llms.txt"
)]
pub struct Cli {
    #[command(subcommand)]
    pub(super) command: Commands,
}

#[derive(Subcommand)]
pub(super) enum Commands {
    /// Infer name from project and run through proxy
    #[command(
        after_help = "Port placeholder:\n  Use NSL_PORT in child command arguments when a CLI does not read PORT.\n  Example: nsl run ./server --port NSL_PORT"
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
        #[arg(short, long)]
        force: bool,

        /// Rewrite Host header to target address
        #[arg(short = 'c', long)]
        change_origin: bool,

        /// Strip the path prefix (from name like `myapp:/api`) before forwarding
        #[arg(short, long)]
        strip: bool,
    },

    /// Start the proxy server
    #[command(
        after_help = "Proxy listen address:\n  Use --listen ADDR or NSL_LISTEN=ADDR to configure where the proxy listens.\n  Examples: nsl start --listen 127.0.0.1:1355\n            NSL_LISTEN=:1355 nsl start"
    )]
    Start {
        /// Listen address for the proxy (e.g. 127.0.0.1:1355 or :1355)
        #[arg(short, long)]
        listen: Option<String>,

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
    #[command(
        after_help = "Proxy listen address:\n  Use --listen ADDR or NSL_LISTEN=ADDR to configure where the proxy listens.\n  Examples: nsl reload --listen 127.0.0.1:1355\n            NSL_LISTEN=:1355 nsl reload"
    )]
    Reload {
        /// Listen address for the proxy (e.g. 127.0.0.1:1355 or :1355)
        #[arg(short, long)]
        listen: Option<String>,

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
    #[command(
        after_help = "Examples:\n  nsl route api 3001\n  nsl route shop:/api 3001 --strip\n  nsl route api --remove\n\nPath prefixes:\n  NAME:/PATH mounts a service under one host.\n  --strip removes the matched prefix before forwarding, so /api/users becomes /users."
    )]
    Route {
        /// App name, optionally with a path prefix (e.g. `myapp` or `myapp:/api`)
        name: Option<String>,

        /// Target port
        port: Option<u16>,

        /// Remove the route
        #[arg(long)]
        remove: bool,

        /// Override an existing route
        #[arg(short, long)]
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
    listen: Option<String>,
    https: bool,
) -> anyhow::Result<()> {
    if let Some(raw) = listen {
        match crate::config::parse_listen(&raw) {
            Ok((bind, port)) => {
                config.proxy_bind = bind;
                config.proxy_port = port;
            }
            Err(e) => {
                anyhow::bail!("invalid --listen '{}': {}", raw, e);
            }
        }
    }
    if https {
        config.proxy_https = true;
    }
    Ok(())
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

    #[test]
    fn test_start_help_shows_listen_env() {
        let mut command = Cli::command();
        let start = command.find_subcommand_mut("start").unwrap();
        let mut help = Vec::new();

        start.write_help(&mut help).unwrap();
        let help = String::from_utf8(help).unwrap();

        assert!(help.contains("NSL_LISTEN"));
        assert!(help.contains("nsl start --listen 127.0.0.1:1355"));
    }

    #[test]
    fn test_help_shows_readme_url() {
        let mut command = Cli::command();
        let mut help = Vec::new();

        command.write_help(&mut help).unwrap();
        let help = String::from_utf8(help).unwrap();

        assert!(help.contains("Documentation:"));
        assert!(help.contains("https://github.com/nsiod/nsl/blob/main/README.md"));
        assert!(help.contains("https://github.com/nsiod/nsl/blob/main/llms.txt"));
    }

    #[test]
    fn test_route_help_shows_examples_and_strip() {
        let mut command = Cli::command();
        let route = command.find_subcommand_mut("route").unwrap();
        let mut help = Vec::new();

        route.write_help(&mut help).unwrap();
        let help = String::from_utf8(help).unwrap();

        assert!(help.contains("nsl route api 3001"));
        assert!(help.contains("nsl route shop:/api 3001 --strip"));
        assert!(help.contains("/api/users becomes /users"));
    }

    #[test]
    fn test_force_has_short_flag() {
        let mut command = Cli::command();

        let run = command.find_subcommand_mut("run").unwrap();
        let mut run_help = Vec::new();
        run.write_help(&mut run_help).unwrap();
        let run_help = String::from_utf8(run_help).unwrap();
        assert!(run_help.contains("-f, --force"));

        let route = command.find_subcommand_mut("route").unwrap();
        let mut route_help = Vec::new();
        route.write_help(&mut route_help).unwrap();
        let route_help = String::from_utf8(route_help).unwrap();
        assert!(route_help.contains("-f, --force"));
    }
}

use crate::config;

use super::{Cli, Commands, HostsAction, TunnelAction, apply_cli_overrides};

async fn start_proxy(config: &crate::config::Config, foreground: bool) -> anyhow::Result<()> {
    if foreground {
        crate::proxy::start_proxy(config).await
    } else {
        if crate::proxy::ensure_proxy_running(config).await? {
            println!("proxy started on http://localhost:{}", config.proxy_port);
        } else {
            println!("proxy is already running on port {}", config.proxy_port);
        }
        Ok(())
    }
}

pub(super) async fn handle(cli: Cli) -> anyhow::Result<()> {
    let mut config = config::load_config();

    match cli.command {
        Commands::Run {
            cmd,
            name,
            app_port,
            force,
            change_origin,
            strip,
        } => {
            if let Some(p) = app_port {
                config.app_port = Some(p);
            }
            if force {
                config.app_force = true;
            }
            let (bare_name, path) = match name.as_deref() {
                Some(raw) => {
                    let (n, p) = crate::utils::split_name_path(raw);
                    (Some(n), p)
                }
                None => (None, "/".to_string()),
            };
            crate::run::run_app(
                &config,
                &cmd,
                bare_name.as_deref(),
                change_origin,
                &path,
                strip,
            )
            .await
        }
        Commands::Start {
            listen,
            https,
            foreground,
            daemonize,
        } => {
            apply_cli_overrides(&mut config, listen, https)?;

            if daemonize {
                crate::proxy::daemonize_and_start_proxy(&config)
            } else {
                start_proxy(&config, foreground).await
            }
        }
        Commands::Stop => {
            let state_dir = config.resolve_state_dir();
            crate::proxy::stop_proxy(&state_dir)
        }
        Commands::Reload {
            listen,
            https,
            foreground,
        } => {
            apply_cli_overrides(&mut config, listen, https)?;
            let state_dir = config.resolve_state_dir();
            // Best-effort stop; ignore if not running.
            let _ = crate::proxy::stop_proxy(&state_dir);
            start_proxy(&config, foreground).await
        }
        Commands::Logs { follow, lines } => {
            let state_dir = config.resolve_state_dir();
            crate::proxy::show_logs(&state_dir, follow, lines).await
        }
        Commands::Route {
            name,
            port,
            remove,
            force,
            change_origin,
            strip,
        } => {
            let state_dir = config.resolve_state_dir();
            let store = crate::routes::RouteStore::new(state_dir);

            if remove {
                let raw =
                    name.ok_or_else(|| anyhow::anyhow!("route name is required for --remove"))?;
                let (bare_name, path) = crate::utils::split_name_path(&raw);
                let hostname = crate::utils::parse_hostname(&bare_name, &config.domains)
                    .map_err(|e| anyhow::anyhow!("{}", e))?;
                let path_filter = if path == "/" {
                    None
                } else {
                    Some(path.as_str())
                };
                store.remove_route(&hostname, path_filter)?;
                println!("removed route: {}", hostname);
            } else {
                let raw = name.ok_or_else(|| anyhow::anyhow!("route name is required"))?;
                let port = port.ok_or_else(|| anyhow::anyhow!("target port is required"))?;
                let (bare_name, path) = crate::utils::split_name_path(&raw);
                let hostname = crate::utils::parse_hostname(&bare_name, &config.domains)
                    .map_err(|e| anyhow::anyhow!("{}", e))?;
                store.add_route(&hostname, port, 0, force, change_origin, &path, strip)?;
                let url = crate::utils::format_url(
                    &hostname,
                    config.proxy_port,
                    config.proxy_https,
                    &config.domain_displays,
                );
                let path_info = if path != "/" { path.as_str() } else { "" };
                println!("{}{} -> localhost:{}", url, path_info, port);
            }
            Ok(())
        }
        Commands::Get { name } => {
            let (bare_name, path) = crate::utils::split_name_path(&name);
            let hostname = crate::utils::parse_hostname(&bare_name, &config.domains)
                .map_err(|e| anyhow::anyhow!("{}", e))?;
            let url = crate::utils::format_url(
                &hostname,
                config.proxy_port,
                config.proxy_https,
                &config.domain_displays,
            );
            let path_suffix = if path != "/" { path.as_str() } else { "" };
            println!("{}{}", url, path_suffix);
            Ok(())
        }
        Commands::List => {
            let state_dir = config.resolve_state_dir();
            let store = crate::routes::RouteStore::new(state_dir);
            let routes = store.load_routes()?;
            if routes.is_empty() {
                println!("No active routes.");
            } else {
                for route in &routes {
                    let url = crate::utils::format_url(
                        &route.hostname,
                        config.proxy_port,
                        config.proxy_https,
                        &config.domain_displays,
                    );
                    let path_info = if route.path_prefix != "/" {
                        route.path_prefix.as_str()
                    } else {
                        ""
                    };
                    let mut flags = Vec::new();
                    if route.change_origin {
                        flags.push("change_origin");
                    }
                    if route.strip_prefix && route.path_prefix != "/" {
                        flags.push("strip_prefix");
                    }
                    let flags_str = if flags.is_empty() {
                        String::new()
                    } else {
                        format!(" [{}]", flags.join(", "))
                    };
                    println!(
                        "  {}{} -> localhost:{}  (pid {}){}",
                        url, path_info, route.port, route.pid, flags_str
                    );
                }
            }
            Ok(())
        }
        Commands::Status => {
            crate::status::print_status();
            println!();
            config::print_config();
            Ok(())
        }
        Commands::Trust => {
            let state_dir = config.resolve_state_dir();
            match crate::certs::ensure_certs(&state_dir) {
                Ok(paths) => {
                    println!("CA certificate: {}", paths.ca_cert.display());
                }
                Err(e) => {
                    anyhow::bail!("failed to generate certificates: {}", e);
                }
            }

            println!("installing CA into system trust store...");
            match crate::certs::trust_ca(&state_dir)? {
                crate::certs::TrustResult::AlreadyTrusted => {
                    println!("CA is already trusted by the system.");
                }
                crate::certs::TrustResult::Installed => {
                    println!("CA installed successfully. HTTPS is now available.");
                }
                crate::certs::TrustResult::PermissionDenied(msg) => {
                    anyhow::bail!("{}", msg);
                }
                crate::certs::TrustResult::Failed(msg) => {
                    anyhow::bail!("failed to install CA: {}", msg);
                }
            }
            Ok(())
        }
        Commands::Hosts { action } => match action {
            HostsAction::Sync => {
                let state_dir = config.resolve_state_dir();
                let store = crate::routes::RouteStore::new(state_dir);
                let routes = store.load_routes()?;
                let hostnames = crate::hosts::collect_hostnames_from_routes(&routes);

                if hostnames.is_empty() {
                    println!("no routes registered, nothing to sync");
                    return Ok(());
                }

                let hosts_path = std::path::PathBuf::from("/etc/hosts");
                match crate::hosts::sync_hosts_file(&hostnames, &hosts_path) {
                    Ok(()) => {
                        println!("synced {} hostname(s) to /etc/hosts:", hostnames.len());
                        for h in &hostnames {
                            println!("  127.0.0.1 {}", h);
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                        anyhow::bail!(
                            "permission denied writing /etc/hosts. Try: sudo nsl hosts sync"
                        );
                    }
                    Err(e) => return Err(e.into()),
                }
                Ok(())
            }
            HostsAction::Clean => {
                let hosts_path = std::path::PathBuf::from("/etc/hosts");
                match crate::hosts::clean_hosts_file(&hosts_path) {
                    Ok(()) => {
                        println!("removed nsl entries from /etc/hosts");
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                        anyhow::bail!(
                            "permission denied writing /etc/hosts. Try: sudo nsl hosts clean"
                        );
                    }
                    Err(e) => return Err(e.into()),
                }
                Ok(())
            }
        },
        Commands::Tunnel { action } => match action {
            TunnelAction::Connect { endpoint, id, key } => {
                if let Some(v) = endpoint {
                    config.tunnel.endpoint = Some(v);
                }
                if let Some(v) = id {
                    config.tunnel.id = Some(v);
                }
                if let Some(v) = key {
                    config.tunnel.key = Some(v);
                }
                config.tunnel.enable = true;
                crate::tunnel::connect_once(&config).await
            }
            TunnelAction::Status => {
                crate::tunnel::print_status(&config);
                Ok(())
            }
        },
    }
}

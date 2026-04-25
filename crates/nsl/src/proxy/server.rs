use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use hyper::Request;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use hyper_util::server::conn::auto::Builder as AutoBuilder;
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;

use crate::config::Config;
use crate::routes::{RouteMapping, RouteStore};
use crate::utils::fix_ownership;

use super::RouteCache;
use super::handler::handle_request;

/// Interval between route file mtime checks (in seconds).
const ROUTE_POLL_INTERVAL_SECS: u64 = 2;

// ---------------------------------------------------------------------------
// Lifecycle files
// ---------------------------------------------------------------------------

/// Write PID and port files to the state directory.
fn write_lifecycle_files(state_dir: &Path, port: u16) -> anyhow::Result<()> {
    fs::create_dir_all(state_dir)?;
    fix_ownership(state_dir);
    fs::write(state_dir.join("proxy.pid"), std::process::id().to_string())?;
    fix_ownership(&state_dir.join("proxy.pid"));
    fs::write(state_dir.join("proxy.port"), port.to_string())?;
    fix_ownership(&state_dir.join("proxy.port"));
    tracing::debug!("wrote proxy.pid and proxy.port to {}", state_dir.display());
    Ok(())
}

/// Remove PID and port files from the state directory.
pub(super) fn cleanup_lifecycle_files(state_dir: &Path) {
    let _ = fs::remove_file(state_dir.join("proxy.pid"));
    let _ = fs::remove_file(state_dir.join("proxy.port"));
    let _ = fs::remove_file(state_dir.join("proxy.tls"));
    tracing::debug!("cleaned up lifecycle files in {}", state_dir.display());
}

/// Write TLS marker file if HTTPS is enabled.
fn write_tls_marker(state_dir: &Path, tls: bool) -> anyhow::Result<()> {
    let marker = state_dir.join("proxy.tls");
    if tls {
        fs::write(marker, "1")?;
    } else {
        let _ = fs::remove_file(marker);
    }
    Ok(())
}

/// Load routes from disk via the RouteStore, returning an empty vec on error.
fn load_routes_from_disk(state_dir: &Path) -> Vec<RouteMapping> {
    let store = RouteStore::new(state_dir.to_path_buf());
    match store.load_routes() {
        Ok(routes) => routes,
        Err(e) => {
            tracing::warn!("failed to load routes from {}: {}", state_dir.display(), e);
            Vec::new()
        }
    }
}

/// Get the mtime of a file, returning `None` if the file doesn't exist or
/// metadata cannot be read.
fn file_mtime(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).ok()?.modified().ok()
}

/// Spawn a background task that polls routes.json mtime every
/// `ROUTE_POLL_INTERVAL_SECS` seconds. When the mtime changes (or the file
/// is created/deleted), the task reloads routes into the shared cache.
fn spawn_route_poller(state_dir: PathBuf, cache: RouteCache) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let routes_path = state_dir.join("routes.json");
        let mut last_mtime: Option<SystemTime> = file_mtime(&routes_path);

        loop {
            tokio::time::sleep(std::time::Duration::from_secs(ROUTE_POLL_INTERVAL_SECS)).await;

            let current_mtime = file_mtime(&routes_path);
            if current_mtime != last_mtime {
                let new_routes = load_routes_from_disk(&state_dir);
                tracing::debug!(
                    "routes.json changed, reloaded {} route(s)",
                    new_routes.len()
                );
                let mut w = cache.write().await;
                *w = new_routes;
                last_mtime = current_mtime;
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Server start / TLS / serve
// ---------------------------------------------------------------------------

/// Start the proxy server with the given config.
pub async fn start_proxy(config: &Config) -> anyhow::Result<()> {
    let port = config.proxy_port;
    let max_hops = config.max_hops as u32;
    let addr = SocketAddr::new(config.proxy_bind, port);
    let listener = TcpListener::bind(addr).await?;

    let state_dir = config.resolve_state_dir();
    let store = RouteStore::new(state_dir.clone());
    store.ensure_dir()?;

    // Set up TLS acceptor if HTTPS is enabled.
    let tls_acceptor: Option<TlsAcceptor> = if config.proxy_https {
        let cert_paths = crate::certs::ensure_certs(&state_dir)?;
        let tls_config = crate::certs::build_tls_server_config(&cert_paths, state_dir.clone())?;
        Some(TlsAcceptor::from(Arc::new(tls_config)))
    } else {
        None
    };

    // Write lifecycle files
    write_lifecycle_files(&state_dir, port)?;
    write_tls_marker(&state_dir, config.proxy_https)?;

    // Initialize the in-memory route cache with current disk state
    let initial_routes = load_routes_from_disk(&state_dir);
    let route_cache: RouteCache = Arc::new(tokio::sync::RwLock::new(initial_routes));
    // Mutable allow-list: starts with the configured static domains and
    // gets extended at runtime when the tunnel server assigns a tenant
    // domain to this client.
    let domains: crate::tunnel::SharedDomains =
        Arc::new(std::sync::RwLock::new(config.domains.clone()));

    // Spawn background task that polls routes.json for changes
    let poller_handle = spawn_route_poller(state_dir.clone(), Arc::clone(&route_cache));

    // Spawn QUIC tunnel client if enabled. Runs for the lifetime of
    // the daemon and handles its own reconnect/backoff. `tunnel_cancel`
    // is triggered on shutdown to break the reconnect loop cleanly.
    let tunnel_cancel = tokio_util::sync::CancellationToken::new();
    let tunnel_handle =
        crate::tunnel::spawn_client_task(config, Arc::clone(&domains), tunnel_cancel.clone());
    if tunnel_handle.is_some()
        && let Some(ref id) = config.tunnel.id
    {
        tracing::info!("tunnel client enabled for id {}", id);
    }

    let proto = if config.proxy_https { "https" } else { "http" };
    tracing::info!("proxy listening on {}://{}:{}", proto, addr.ip(), port);

    // Set up graceful shutdown on SIGINT/SIGTERM (Unix) or Ctrl+C / Ctrl+Break
    // (Windows). On Windows, `tokio::signal::ctrl_c` handles both the
    // console Ctrl+C and Ctrl+Break events that we receive despite the
    // DETACHED_PROCESS flag (via attached job/console signals).
    let state_dir_cleanup = state_dir.clone();
    #[cfg(unix)]
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|e| anyhow::anyhow!("failed to register SIGTERM handler: {e}"))?;
    let shutdown = async move {
        let ctrl_c = tokio::signal::ctrl_c();

        #[cfg(unix)]
        tokio::select! {
            _ = ctrl_c => {
                tracing::info!("received SIGINT, shutting down");
            }
            _ = sigterm.recv() => {
                tracing::info!("received SIGTERM, shutting down");
            }
        }

        #[cfg(not(unix))]
        {
            let _ = ctrl_c.await;
            tracing::info!("received Ctrl+C, shutting down");
        }

        cleanup_lifecycle_files(&state_dir_cleanup);
    };

    tokio::select! {
        _ = async {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let cache = Arc::clone(&route_cache);
                        let domains = Arc::clone(&domains);
                        let acceptor = tls_acceptor.clone();
                        tokio::task::spawn(async move {
                            if let Some(acceptor) = acceptor {
                                handle_connection_with_tls(stream, acceptor, port, max_hops, cache, domains).await;
                            } else {
                                let io = TokioIo::new(stream);
                                serve_http(io, port, max_hops, cache, domains, false).await;
                            }
                        });
                    }
                    Err(err) => {
                        tracing::error!("accept error: {}", err);
                    }
                }
            }
        } => {}
        _ = shutdown => {}
    }

    // Stop the background poller
    poller_handle.abort();
    // Signal the tunnel client to close cleanly; wait briefly for it.
    tunnel_cancel.cancel();
    if let Some(h) = tunnel_handle {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), h).await;
    }

    // Ensure cleanup on normal exit
    cleanup_lifecycle_files(&state_dir);
    tracing::info!("proxy stopped");
    Ok(())
}

/// Handle a connection that may be TLS or plain HTTP on the same port.
/// Peek the first byte: 0x16 indicates a TLS ClientHello.
async fn handle_connection_with_tls(
    stream: TcpStream,
    acceptor: TlsAcceptor,
    port: u16,
    max_hops: u32,
    route_cache: RouteCache,
    domains: crate::tunnel::SharedDomains,
) {
    let mut peek_buf = [0u8; 1];
    match stream.peek(&mut peek_buf).await {
        Ok(0) => return, // connection closed
        Ok(_) => {}
        Err(e) => {
            tracing::debug!("peek error: {}", e);
            return;
        }
    }

    if peek_buf[0] == 0x16 {
        // TLS handshake
        match acceptor.accept(stream).await {
            Ok(tls_stream) => {
                let io = TokioIo::new(tls_stream);
                serve_http(io, port, max_hops, route_cache, domains, true).await;
            }
            Err(e) => {
                tracing::debug!("TLS handshake error: {}", e);
            }
        }
    } else {
        // Plain HTTP on a TLS-enabled port
        let io = TokioIo::new(stream);
        serve_http(io, port, max_hops, route_cache, domains, false).await;
    }
}

/// Serve HTTP/1.1 or HTTP/2 on an already-accepted I/O stream.
async fn serve_http<I>(
    io: I,
    port: u16,
    max_hops: u32,
    route_cache: RouteCache,
    domains: crate::tunnel::SharedDomains,
    is_tls: bool,
) where
    I: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
{
    let service = service_fn(move |req: Request<Incoming>| {
        let cache = Arc::clone(&route_cache);
        let domains = Arc::clone(&domains);
        async move { handle_request(req, port, max_hops, cache, domains, is_tls).await }
    });
    if let Err(err) = AutoBuilder::new(hyper_util::rt::TokioExecutor::new())
        .serve_connection_with_upgrades(io, service)
        .await
    {
        tracing::error!("connection error: {}", err);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_proxy_port_default() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        assert_ne!(addr.port(), 0);
    }

    #[test]
    fn test_write_lifecycle_files_creates_pid_and_port() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path();
        write_lifecycle_files(state_dir, 1355).unwrap();

        let pid_content = fs::read_to_string(state_dir.join("proxy.pid")).unwrap();
        let port_content = fs::read_to_string(state_dir.join("proxy.port")).unwrap();

        assert_eq!(pid_content, std::process::id().to_string());
        assert_eq!(port_content, "1355");
    }

    #[test]
    fn test_cleanup_lifecycle_files_removes_all() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path();

        fs::write(state_dir.join("proxy.pid"), "12345").unwrap();
        fs::write(state_dir.join("proxy.port"), "1355").unwrap();
        fs::write(state_dir.join("proxy.tls"), "1").unwrap();

        cleanup_lifecycle_files(state_dir);

        assert!(!state_dir.join("proxy.pid").exists());
        assert!(!state_dir.join("proxy.port").exists());
        assert!(!state_dir.join("proxy.tls").exists());
    }

    #[test]
    fn test_cleanup_lifecycle_files_no_error_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        cleanup_lifecycle_files(dir.path());
    }

    #[test]
    fn test_write_tls_marker_creates_file_when_enabled() {
        let dir = tempfile::tempdir().unwrap();
        write_tls_marker(dir.path(), true).unwrap();
        assert!(dir.path().join("proxy.tls").exists());
        let content = fs::read_to_string(dir.path().join("proxy.tls")).unwrap();
        assert_eq!(content, "1");
    }

    #[test]
    fn test_write_tls_marker_removes_file_when_disabled() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("proxy.tls"), "1").unwrap();
        write_tls_marker(dir.path(), false).unwrap();
        assert!(!dir.path().join("proxy.tls").exists());
    }
}

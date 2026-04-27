//! Client-side glue between the `nsl` daemon's `Config` and the
//! `nsl-tunnel` library. Provides helpers to spawn the tunnel client task
//! from the proxy daemon, run a one-shot `tunnel connect`, and print
//! client-side status.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::{Config, TunnelConfig};

const RECONNECT_INITIAL_BACKOFF_MS: u64 = 1_000;
const RECONNECT_MAX_BACKOFF_MS: u64 = 30_000;

/// Mutable proxy domains list — shared between the proxy listener's
/// per-request handler (read side) and the tunnel client (write side,
/// triggered by the server's domain assignment).
pub type SharedDomains = Arc<RwLock<Vec<String>>>;

/// Spawn the tunnel client as a long-running task under the proxy daemon.
/// Returns `None` when the tunnel is disabled or missing required fields.
/// The returned task exits promptly when `cancel` is triggered. The
/// shared `domains` list gets extended whenever the server assigns a
/// new tenant domain so route registration for that host works
/// immediately without a daemon restart.
pub fn spawn_client_task(
    config: &Config,
    domains: SharedDomains,
    cancel: CancellationToken,
) -> Option<JoinHandle<()>> {
    if !config.tunnel.enable {
        return None;
    }
    let client = match build_client_tunnel(&config.tunnel) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "tunnel enabled but config invalid; skipping");
            return None;
        }
    };
    let proxy_port = config.proxy_port;

    Some(tokio::spawn(async move {
        run_reconnect_loop(client, proxy_port, domains, cancel).await;
    }))
}

/// Single-shot connect used by `nsl tunnel connect`: runs until the
/// session closes, then returns.
pub async fn connect_once(config: &Config) -> Result<()> {
    let client = build_client_tunnel(&config.tunnel)?;
    ::tunnel::client::run(client, config.proxy_port).await
}

/// Print resolved tunnel-client config.
pub fn print_status(config: &Config) {
    println!("Tunnel (client)");
    println!("  enable:   {}", config.tunnel.enable);
    if let Some(ref id) = config.tunnel.id {
        println!("  id:       {}", id);
    }
    if let Some(ref e) = config.tunnel.endpoint {
        println!("  endpoint: {}", e);
    }
    if config.tunnel.key.is_some() {
        println!("  key:      <set>");
    }
    if let Some(ref fp) = config.tunnel.server_id {
        println!("  server_id: {}", fp);
    }
    println!("  proxy:    127.0.0.1:{}", config.proxy_port);
}

fn build_client_tunnel(t: &TunnelConfig) -> Result<::tunnel::ClientTunnel> {
    let id =
        t.id.clone()
            .ok_or_else(|| anyhow!("tunnel.id is required"))?;
    let key = t
        .key
        .clone()
        .ok_or_else(|| anyhow!("tunnel.key is required"))?;
    let endpoint = t
        .endpoint
        .clone()
        .ok_or_else(|| anyhow!("tunnel.endpoint is required"))?;
    let fingerprint = t
        .server_id
        .as_deref()
        .ok_or_else(|| anyhow!("tunnel.server_id is required"))?;
    ::tunnel::ClientTunnel::new(id, key, endpoint, fingerprint)
        .context("invalid tunnel client config")
}

async fn run_reconnect_loop(
    client: ::tunnel::ClientTunnel,
    proxy_port: u16,
    domains: SharedDomains,
    cancel: CancellationToken,
) {
    // Build the QUIC endpoint once; the rustls session cache lives on it,
    // so keeping it stable across reconnects is what unlocks 0-RTT on the
    // second and subsequent dials.
    let endpoint = match ::tunnel::client::build_endpoint(&client) {
        Ok(e) => e,
        Err(e) => {
            tracing::error!(error = %e, "failed to build tunnel endpoint; giving up");
            return;
        }
    };

    // Hook fired on every successful HelloAck. Appends the
    // server-assigned domain to the proxy's allowed-domains list if
    // it isn't already there.
    let hook: ::tunnel::client::AssignedHook = {
        let domains = domains.clone();
        Arc::new(move |domain: &str| {
            let d = domain.to_ascii_lowercase();
            if d.is_empty() {
                return;
            }
            let mut w = match domains.write() {
                Ok(w) => w,
                Err(_) => {
                    tracing::warn!("proxy domains lock poisoned; skipping tunnel-assigned domain");
                    return;
                }
            };
            if w.iter().any(|s| s == &d) {
                return;
            }
            w.push(d.clone());
            tracing::info!(domain = %d, "added tunnel-assigned domain to proxy allow-list");
        })
    };

    let mut backoff_ms = RECONNECT_INITIAL_BACKOFF_MS;
    loop {
        if cancel.is_cancelled() {
            return;
        }
        let session_cancel = cancel.clone();
        let run_fut = ::tunnel::client::run_on_endpoint(
            &endpoint,
            client.clone(),
            proxy_port,
            Some(hook.clone()),
            session_cancel,
        );
        match run_fut.await {
            Ok(()) => {
                tracing::info!("tunnel session ended; reconnecting");
                backoff_ms = RECONNECT_INITIAL_BACKOFF_MS;
            }
            Err(e) => {
                tracing::warn!(error = %e, backoff_ms, "tunnel session error; backing off");
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(backoff_ms)) => {}
            _ = cancel.cancelled() => return,
        }
        backoff_ms = (backoff_ms * 2).min(RECONNECT_MAX_BACKOFF_MS);
    }
}

//! QUIC tunnel client.
//!
//! Responsibilities:
//! - Dial the configured server endpoint over QUIC.
//! - Open a bidirectional control stream and send `Hello`.
//! - Verify the server's HMAC proof, then prove our own identity.
//! - Pump a data plane: server-opened bi-streams forward to the local
//!   proxy port.
//! - Keep the control stream alive with periodic Ping until cancelled or
//!   the connection is closed.
//!
//! The [`Endpoint`] is created once and reused across reconnects so that
//! TLS 1.3 session tickets accumulate in the rustls client cache. That
//! enables the opportunistic 0-RTT fast path in [`run_on_endpoint`] —
//! after the first successful handshake, subsequent reconnects skip the
//! extra RTT of a full TLS handshake.

use std::net::{Ipv4Addr, SocketAddr, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use quinn::Endpoint;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;

use crate::config::ClientTunnel;
use crate::constants::{HANDSHAKE_TIMEOUT_SECS, PING_INTERVAL_CAP_SECS};
use crate::protocol::{
    self, ControlFrame, PROTOCOL_VERSION, ProtocolError, read_frame, write_frame,
};
use crate::tls::{TLS_SERVER_NAME, build_client_crypto};

#[derive(Debug, Clone)]
pub struct ConnectedSession {
    pub session_id: String,
    pub assigned_domain: String,
    pub server_keepalive_secs: u32,
}

/// Called once per session right after the server issues `HelloAck`,
/// with the public domain the server just assigned to this client. The
/// caller typically uses this to extend its local proxy's allowed-
/// domains list so route registration works for the assigned name.
pub type AssignedHook = Arc<dyn Fn(&str) + Send + Sync>;

/// Build a reusable QUIC endpoint configured to authenticate the server
/// against `cfg.server_id`. Share this across reconnects so TLS session
/// tickets are preserved and 0-RTT is available on the second dial.
pub fn build_endpoint(cfg: &ClientTunnel) -> Result<Endpoint> {
    install_ring_provider_once();
    let crypto = build_client_crypto(cfg.server_id)?;
    let client_config = quinn::ClientConfig::new(crypto);
    let mut endpoint =
        Endpoint::client(SocketAddr::from(([0, 0, 0, 0], 0))).context("creating QUIC endpoint")?;
    endpoint.set_default_client_config(client_config);
    Ok(endpoint)
}

/// Dial the server, perform the handshake, then park on the control
/// stream until it closes. Returns `Ok` on graceful close, `Err` on
/// any failure (including auth errors).
///
/// Convenience wrapper that builds a throw-away endpoint. Long-lived
/// callers should build an endpoint via [`build_endpoint`] once and
/// repeatedly call [`run_on_endpoint`] to benefit from 0-RTT.
pub async fn run(cfg: ClientTunnel, proxy_port: u16) -> Result<()> {
    run_with_cancel(cfg, proxy_port, CancellationToken::new()).await
}

/// Variant of `run` that exits promptly when `cancel` is triggered,
/// closing the QUIC connection cleanly.
pub async fn run_with_cancel(
    cfg: ClientTunnel,
    proxy_port: u16,
    cancel: CancellationToken,
) -> Result<()> {
    let endpoint = build_endpoint(&cfg)?;
    run_on_endpoint(&endpoint, cfg, proxy_port, None, cancel).await
}

/// Run a single tunnel session over `endpoint`. Attempts 0-RTT first; if
/// the server has no cached session ticket for us we transparently fall
/// back to a full 1-RTT handshake. `assigned_hook`, if provided, fires
/// once per session as soon as the server issues the domain assignment
/// in `HelloAck`.
pub async fn run_on_endpoint(
    endpoint: &Endpoint,
    cfg: ClientTunnel,
    proxy_port: u16,
    assigned_hook: Option<AssignedHook>,
    cancel: CancellationToken,
) -> Result<()> {
    let remote_addr = resolve_endpoint(&cfg.endpoint)?;

    tracing::info!(
        endpoint = %cfg.endpoint,
        remote = %remote_addr,
        client_id = %cfg.client_id,
        "dialing tunnel server"
    );

    let connecting = endpoint
        .connect(remote_addr, TLS_SERVER_NAME)
        .with_context(|| format!("QUIC connect({})", remote_addr))?;

    // Try 0-RTT; on first connect or after ticket expiry this returns
    // `Err(Connecting)` and we fall back to a 1-RTT handshake.
    let connection = match connecting.into_0rtt() {
        Ok((conn, zero_rtt_accepted)) => {
            tracing::debug!("tunnel client: 0-RTT path");
            // Confirm 0-RTT acceptance in the background so our stream
            // work doesn't block on the 1-RTT completion.
            tokio::spawn(async move {
                let accepted = zero_rtt_accepted.await;
                tracing::debug!(accepted, "0-RTT resolution");
            });
            conn
        }
        Err(connecting) => {
            tracing::debug!("tunnel client: 1-RTT handshake");
            tokio::time::timeout(Duration::from_secs(HANDSHAKE_TIMEOUT_SECS), connecting)
                .await
                .context("QUIC TLS handshake timed out")?
                .context("QUIC TLS handshake failed")?
        }
    };

    let (mut send, mut recv) = connection
        .open_bi()
        .await
        .context("opening control stream")?;

    let client_nonce = protocol::random_nonce()
        .map_err(|_| anyhow::anyhow!("RNG failure generating client nonce"))?;

    write_frame(
        &mut send,
        &ControlFrame::Hello {
            version: PROTOCOL_VERSION,
            client_id: cfg.client_id.clone(),
            client_nonce,
        },
    )
    .await
    .context("writing Hello frame")?;

    // Read Challenge and verify that the server knows the shared key.
    let challenge = tokio::time::timeout(
        Duration::from_secs(HANDSHAKE_TIMEOUT_SECS),
        read_frame(&mut recv),
    )
    .await
    .context("Challenge timeout")?
    .context("reading Challenge")?;

    let (server_nonce, server_proof) = match challenge {
        ControlFrame::Challenge {
            server_nonce,
            server_proof,
        } => (server_nonce, server_proof),
        ControlFrame::HelloErr { reason } => {
            anyhow::bail!("tunnel handshake rejected: {}", reason);
        }
        other => anyhow::bail!("expected Challenge, got {:?}", other),
    };

    if !protocol::verify(cfg.key.as_bytes(), &client_nonce, &server_proof) {
        anyhow::bail!("server failed authentication: bad HMAC (wrong key or MITM)");
    }

    // Server proved it knows the key; now prove we know it too.
    let digest = protocol::sign(cfg.key.as_bytes(), &server_nonce);
    write_frame(&mut send, &ControlFrame::AuthResponse { digest })
        .await
        .context("writing AuthResponse")?;

    let ack = tokio::time::timeout(
        Duration::from_secs(HANDSHAKE_TIMEOUT_SECS),
        read_frame(&mut recv),
    )
    .await
    .context("HelloAck timeout")?
    .context("reading HelloAck")?;

    let session = match ack {
        ControlFrame::HelloAck {
            session_id,
            assigned_domain,
            keepalive_secs,
        } => ConnectedSession {
            session_id,
            assigned_domain,
            server_keepalive_secs: keepalive_secs,
        },
        ControlFrame::HelloErr { reason } => {
            anyhow::bail!("tunnel authentication failed: {}", reason);
        }
        other => anyhow::bail!("unexpected handshake frame: {:?}", other),
    };

    tracing::info!(
        session = %session.session_id,
        assigned_domain = %session.assigned_domain,
        keepalive = session.server_keepalive_secs,
        "tunnel session established"
    );
    println!(
        "tunnel connected: id={} assigned_domain={} session={}",
        cfg.client_id, session.assigned_domain, session.session_id
    );

    if let Some(hook) = &assigned_hook {
        hook(&session.assigned_domain);
    }

    // Spawn data-plane accept loop: each server-initiated bi-stream is
    // a public HTTP request that we forward to 127.0.0.1:proxy_port.
    let data_conn = connection.clone();
    let data_task = tokio::spawn(async move {
        run_data_plane(data_conn, proxy_port).await;
    });

    // Park on ping loop + read loop. Exit when the server closes.
    let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);

    let ping_interval = Duration::from_secs(
        PING_INTERVAL_CAP_SECS.min(session.server_keepalive_secs.max(1) as u64),
    );
    let ping_task = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(ping_interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            if write_frame(&mut send, &ControlFrame::Ping).await.is_err() {
                let _ = shutdown_tx.send(()).await;
                return;
            }
        }
    });

    let recv_loop = async {
        loop {
            match read_frame(&mut recv).await {
                Ok(ControlFrame::Pong) => {}
                Ok(ControlFrame::Ping) => {
                    // Server-initiated ping: we'd need another send half
                    // to reply. For MVP, silently ignore — server's own
                    // control loop only replies to our Ping.
                }
                Ok(other) => tracing::debug!("ignoring control frame: {:?}", other),
                Err(ProtocolError::UnexpectedEof) | Err(ProtocolError::Io(_)) => return,
                Err(e) => {
                    tracing::warn!(error = %e, "control stream error");
                    return;
                }
            }
        }
    };

    tokio::select! {
        _ = recv_loop => {}
        _ = shutdown_rx.recv() => {}
        _ = cancel.cancelled() => {
            tracing::info!("tunnel client cancelled; closing");
            connection.close(0u32.into(), b"client shutdown");
        }
    }
    ping_task.abort();
    data_task.abort();

    tracing::info!("tunnel session closed");
    Ok(())
}

/// Accept server-initiated bi-streams and pump each one through to the
/// local proxy at `127.0.0.1:proxy_port`. Exits when the connection
/// closes.
async fn run_data_plane(connection: quinn::Connection, proxy_port: u16) {
    loop {
        match connection.accept_bi().await {
            Ok((send, recv)) => {
                tokio::spawn(async move {
                    if let Err(e) = forward_to_local(send, recv, proxy_port).await {
                        tracing::debug!(error = %e, "data stream ended");
                    }
                });
            }
            Err(e) => {
                tracing::debug!(error = %e, "data-plane accept loop ending");
                return;
            }
        }
    }
}

async fn forward_to_local(
    mut send: quinn::SendStream,
    mut recv: quinn::RecvStream,
    proxy_port: u16,
) -> Result<()> {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, proxy_port));
    let stream = TcpStream::connect(addr)
        .await
        .with_context(|| format!("failed to dial local proxy at {}", addr))?;
    let (mut tcp_r, mut tcp_w) = stream.into_split();
    use tokio::io::AsyncWriteExt;

    let to_local = async {
        let _ = tokio::io::copy(&mut recv, &mut tcp_w).await;
        let _ = tcp_w.shutdown().await;
    };
    let to_tunnel = async {
        let _ = tokio::io::copy(&mut tcp_r, &mut send).await;
        let _ = send.finish();
    };
    // Symmetric with public.rs: whichever side finishes first tears down
    // the other, matching HTTP proxy semantics where `Connection: close`
    // on the request means the requester only FINs after reading.
    tokio::select! {
        _ = to_local => {}
        _ = to_tunnel => {}
    }
    Ok(())
}

fn resolve_endpoint(s: &str) -> Result<SocketAddr> {
    let mut iter = s
        .to_socket_addrs()
        .with_context(|| format!("resolving tunnel endpoint {}", s))?;
    iter.next()
        .with_context(|| format!("no address for tunnel endpoint {}", s))
}

fn install_ring_provider_once() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_endpoint_parses_host_port() {
        let a = resolve_endpoint("127.0.0.1:4433").unwrap();
        assert_eq!(a.port(), 4433);
    }

    #[test]
    fn resolve_endpoint_errors_on_garbage() {
        let err = resolve_endpoint("not:a:valid:addr").unwrap_err();
        assert!(err.to_string().contains("resolving"));
    }
}

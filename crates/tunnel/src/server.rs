//! QUIC tunnel server.
//!
//! Responsibilities:
//! - Bind a QUIC endpoint.
//! - Accept each incoming connection, perform the mutual-HMAC handshake,
//!   and either register a live [`Session`] in the caller-supplied
//!   [`SessionRegistry`] or reject.
//! - Keep each control stream alive (ping/pong) until either peer drops.
//!
//! The public data plane (HTTPS listener, ACME, Host-header routing) is
//! orchestrated by the `nsld` binary on top of this crate and does not
//! live here.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use quinn::Endpoint;
use tokio_util::sync::CancellationToken;

use crate::config::ServerTunnel;
use crate::constants::{HANDSHAKE_TIMEOUT_SECS, KEEPALIVE_SECS};
use crate::protocol::{
    self, ControlFrame, PROTOCOL_VERSION, ProtocolError, read_frame, write_frame,
};
use crate::registry::{Session, SessionRegistry};
use crate::tls::{build_server_crypto, format_fingerprint};
use crate::tokens::{SharedTokenStore, TokenStore};

/// Callback invoked exactly once when a tenant session has been fully
/// authenticated and inserted into the registry. The daemon uses this to
/// kick off per-tenant ACME issuance in the background. `domain` is the
/// tenant's apex (e.g. `alice.nsl.example.com`).
pub type SessionHook = Arc<dyn Fn(&str) + Send + Sync>;

/// Run the tunnel server. Blocks on the accept loop until the process
/// is interrupted. `registry` is owned by the caller so the public /
/// HTTPS listener in `nsld` can share it for Host-header routing.
pub async fn run(cfg: ServerTunnel, registry: SessionRegistry) -> Result<()> {
    run_with_cancel(cfg, registry, None, CancellationToken::new()).await
}

/// Run the tunnel server, exiting when `cancel` is triggered. `hook`, if
/// provided, fires after each successful tenant handshake.
pub async fn run_with_cancel(
    cfg: ServerTunnel,
    registry: SessionRegistry,
    hook: Option<SessionHook>,
    cancel: CancellationToken,
) -> Result<()> {
    install_ring_provider_once();

    let tokens = TokenStore::load(Path::new(&cfg.tokens_file))?;
    if tokens.is_empty() {
        tracing::warn!(
            path = %cfg.tokens_file,
            "tunnel server loaded 0 tokens — no clients will be able to authenticate"
        );
    } else {
        tracing::info!(tokens = tokens.len(), "tunnel server loaded tokens");
    }

    let (crypto, fingerprint) = build_server_crypto(&cfg.identity_path)?;
    let fingerprint_hex = format_fingerprint(&fingerprint);
    let server_config = quinn::ServerConfig::with_crypto(crypto);
    let listen: SocketAddr = parse_listen(&cfg.listen)?;
    let endpoint = Endpoint::server(server_config, listen)
        .with_context(|| format!("failed to bind QUIC endpoint on {}", listen))?;
    tracing::info!(
        %listen,
        base_domain = %cfg.base_domain,
        identity = %cfg.identity_path.display(),
        server_id = %fingerprint_hex,
        "tunnel server listening"
    );
    // Echo to stdout so operators capturing stdout during bring-up can
    // paste the id into the client config without digging through
    // structured logs.
    println!("tunnel server id: {}", fingerprint_hex);

    let tokens_path = std::path::PathBuf::from(&cfg.tokens_file);
    let (token_store, reload_task) = crate::tokens::spawn_hot_reload(tokens_path, tokens);

    let accept_loop = async {
        while let Some(incoming) = endpoint.accept().await {
            let tokens = token_store.clone();
            let registry = registry.clone();
            let hook = hook.clone();
            tokio::spawn(async move {
                let remote = incoming.remote_address();
                match incoming.await {
                    Ok(conn) => {
                        if let Err(e) = handle_connection(conn, tokens, registry, hook).await {
                            tracing::warn!(%remote, error = %e, "tunnel connection closed");
                        }
                    }
                    Err(e) => tracing::warn!(%remote, error = %e, "tunnel handshake failed"),
                }
            });
        }
    };

    tokio::select! {
        _ = accept_loop => {}
        _ = cancel.cancelled() => {
            tracing::info!("tunnel server received shutdown signal");
        }
    }

    endpoint.close(0u32.into(), b"server shutting down");
    reload_task.abort();
    endpoint.wait_idle().await;
    Ok(())
}

async fn handle_connection(
    conn: quinn::Connection,
    tokens: SharedTokenStore,
    registry: SessionRegistry,
    hook: Option<SessionHook>,
) -> Result<()> {
    let remote = conn.remote_address();

    // Control stream is the first bidirectional stream the client opens.
    let (mut send, mut recv) = tokio::time::timeout(
        std::time::Duration::from_secs(HANDSHAKE_TIMEOUT_SECS),
        conn.accept_bi(),
    )
    .await
    .context("handshake: timed out waiting for control stream")?
    .context("handshake: failed to accept control stream")?;

    let hello = read_frame(&mut recv).await.context("reading Hello frame")?;
    let (client_id, version, client_nonce) = match hello {
        ControlFrame::Hello {
            version,
            client_id,
            client_nonce,
        } => (client_id, version, client_nonce),
        other => {
            let _ = write_frame(
                &mut send,
                &ControlFrame::HelloErr {
                    reason: "expected Hello".into(),
                },
            )
            .await;
            anyhow::bail!("expected Hello, got {:?}", other);
        }
    };

    if version != PROTOCOL_VERSION {
        let _ = write_frame(
            &mut send,
            &ControlFrame::HelloErr {
                reason: format!("unsupported protocol version: {}", version),
            },
        )
        .await;
        anyhow::bail!("protocol version mismatch");
    }

    // Resolve the client id → (assigned_domain, key). Unknown ids get
    // the same vague error as bad keys so a probing attacker can't
    // enumerate which ids exist.
    let entry = match tokens.lookup(&client_id).await {
        Some(e) => e,
        None => {
            let _ = write_frame(
                &mut send,
                &ControlFrame::HelloErr {
                    reason: "authentication failed".into(),
                },
            )
            .await;
            let _ = send.finish();
            let _ =
                tokio::time::timeout(std::time::Duration::from_millis(200), conn.closed()).await;
            anyhow::bail!("auth failed: unknown client_id {}", client_id);
        }
    };
    let domain = entry.domain;
    let key = entry.key;

    // Mutual HMAC: send HMAC(key, client_nonce) so the client can verify
    // us, plus a fresh nonce we want the client to sign.
    let server_nonce =
        protocol::random_nonce().map_err(|_| anyhow::anyhow!("RNG failure generating nonce"))?;
    let server_proof = protocol::sign(key.as_bytes(), &client_nonce);
    write_frame(
        &mut send,
        &ControlFrame::Challenge {
            server_nonce,
            server_proof,
        },
    )
    .await
    .context("writing Challenge")?;

    let auth = tokio::time::timeout(
        std::time::Duration::from_secs(HANDSHAKE_TIMEOUT_SECS),
        read_frame(&mut recv),
    )
    .await
    .context("AuthResponse timeout")?
    .context("reading AuthResponse")?;

    let digest = match auth {
        ControlFrame::AuthResponse { digest } => digest,
        other => {
            let _ = write_frame(
                &mut send,
                &ControlFrame::HelloErr {
                    reason: "expected AuthResponse".into(),
                },
            )
            .await;
            anyhow::bail!("expected AuthResponse, got {:?}", other);
        }
    };

    if !protocol::verify(key.as_bytes(), &server_nonce, &digest) {
        let _ = write_frame(
            &mut send,
            &ControlFrame::HelloErr {
                reason: "authentication failed".into(),
            },
        )
        .await;
        let _ = send.finish();
        let _ = tokio::time::timeout(std::time::Duration::from_millis(200), conn.closed()).await;
        anyhow::bail!("auth failed for domain {}", domain);
    }

    // Replace any existing session for this domain (preempt policy).
    let session_id = format!("{:x}", rand64());
    let session = Arc::new(Session {
        domain: domain.clone(),
        session_id: session_id.clone(),
        connection: conn.clone(),
    });
    if let Some(old) = registry.insert(Arc::clone(&session)).await {
        tracing::info!(domain = %domain, "preempting previous session");
        old.connection
            .close(0u32.into(), b"preempted by new session");
    }
    tracing::info!(%remote, domain = %domain, session = %session_id, "tunnel session established");

    if let Some(h) = &hook {
        h(&domain);
    }

    write_frame(
        &mut send,
        &ControlFrame::HelloAck {
            session_id: session_id.clone(),
            assigned_domain: domain.clone(),
            keepalive_secs: KEEPALIVE_SECS,
        },
    )
    .await
    .context("writing HelloAck")?;

    let ctrl = ControlLoop {
        send,
        recv,
        domain: domain.clone(),
        session_id,
    };
    let result = ctrl.run().await;

    registry.remove_if_current(&session).await;
    tracing::info!(domain = %domain, "tunnel session closed");
    result
}

struct ControlLoop {
    send: quinn::SendStream,
    recv: quinn::RecvStream,
    domain: String,
    session_id: String,
}

impl ControlLoop {
    async fn run(mut self) -> Result<()> {
        loop {
            match read_frame(&mut self.recv).await {
                Ok(ControlFrame::Ping) => {
                    if let Err(e) = write_frame(&mut self.send, &ControlFrame::Pong).await {
                        return Err(e.into());
                    }
                }
                Ok(ControlFrame::Pong) => {}
                Ok(other) => {
                    tracing::debug!(
                        domain = %self.domain,
                        session = %self.session_id,
                        "ignoring unexpected control frame: {:?}",
                        other
                    );
                }
                Err(ProtocolError::UnexpectedEof) | Err(ProtocolError::Io(_)) => {
                    return Ok(());
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
}

fn parse_listen(s: &str) -> Result<SocketAddr> {
    if let Some(port) = s.strip_prefix(':') {
        let port: u16 = port
            .parse()
            .with_context(|| format!("invalid listen port: {}", s))?;
        return Ok(SocketAddr::from(([0, 0, 0, 0], port)));
    }
    s.parse::<SocketAddr>()
        .with_context(|| format!("invalid listen addr: {}", s))
}

fn rand64() -> u64 {
    // Non-crypto 64-bit id. Good enough for session labels.
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    nanos ^ (std::process::id() as u64).rotate_left(17)
}

/// rustls 0.23 requires a crypto provider to be selected when multiple
/// are available. Our Cargo.toml only enables `ring`, but installing a
/// default explicitly also covers the `builder_with_provider` path used
/// indirectly by quinn. Safe to call multiple times.
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
    fn parse_listen_short_form() {
        let a = parse_listen(":4433").unwrap();
        assert_eq!(a.port(), 4433);
        assert!(a.ip().is_unspecified());
    }

    #[test]
    fn parse_listen_full_socket_addr() {
        let a = parse_listen("127.0.0.1:4433").unwrap();
        assert_eq!(a.to_string(), "127.0.0.1:4433");
    }

    #[test]
    fn parse_listen_errors_on_garbage() {
        assert!(parse_listen("not a port").is_err());
    }
}

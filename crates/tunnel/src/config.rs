//! Validated configuration types for the tunnel client and server.
//!
//! The lib is intentionally decoupled from any particular config-file
//! layout: callers parse their own TOML/env/CLI and build these structs
//! via `ClientTunnel::new` / `ServerTunnel::new`.

use std::path::PathBuf;

use anyhow::{Result, anyhow};

use crate::tls::{FINGERPRINT_LEN, parse_fingerprint};

/// A validated client-side tunnel config.
#[derive(Debug, Clone)]
pub struct ClientTunnel {
    /// Where to dial the tunnel server (`host:port`). The server runs
    /// on the operator's chosen public host; the client's own tenant
    /// domain is learned from the server during the handshake so it
    /// doesn't factor into endpoint resolution here.
    pub endpoint: String,
    /// Client handle the server uses to look up this client's token
    /// entry. Not necessarily a DNS name — any short, stable string
    /// agreed with the operator (e.g. `"alice"`, `"alice-laptop"`).
    pub client_id: String,
    pub key: String,
    /// Server identity the client pins. Under the hood it's
    /// `sha256(server_cert_der)` — a 32-byte tag the operator publishes
    /// out of band. A MITM without the matching cert is rejected at the
    /// TLS layer before the handshake completes.
    pub server_id: [u8; FINGERPRINT_LEN],
}

impl ClientTunnel {
    /// Build a validated client config. `server_id` accepts any format
    /// supported by [`parse_fingerprint`] (plain hex, colon-separated,
    /// or `sha256:` prefixed). `endpoint` is required — we no longer
    /// derive it from the tenant domain because the client doesn't
    /// know its tenant domain until the server assigns one.
    pub fn new(client_id: String, key: String, endpoint: String, server_id: &str) -> Result<Self> {
        if client_id.trim().is_empty() {
            return Err(anyhow!("tunnel.id is required"));
        }
        if key.trim().is_empty() {
            return Err(anyhow!("tunnel.key is required"));
        }
        if endpoint.trim().is_empty() {
            return Err(anyhow!("tunnel.endpoint is required"));
        }
        let server_id =
            parse_fingerprint(server_id).map_err(|e| anyhow!("tunnel.server_id: {}", e))?;
        Ok(Self {
            endpoint,
            client_id,
            key,
            server_id,
        })
    }
}

/// A validated server-side tunnel config.
///
/// Server identity is a long-lived self-signed certificate persisted at
/// `identity_path`. The cert's SHA-256(DER) fingerprint is stable across
/// restarts and is the only piece of identity that clients need to pin.
/// Tenant identity is proven separately by the HMAC challenge-response
/// in `protocol.rs`.
///
/// This struct only covers the QUIC tunnel plane. The public HTTP/HTTPS
/// listener is the daemon's responsibility and is configured separately.
#[derive(Debug, Clone)]
pub struct ServerTunnel {
    /// QUIC listen address. Defaults to `:443` to share the standard
    /// HTTP/3 UDP port with real HTTP/3 traffic.
    pub listen: String,
    /// Base domain the server is authoritative for
    /// (e.g. `nsl.example.com`).
    pub base_domain: String,
    /// Path to tokens TOML file.
    pub tokens_file: String,
    /// Path to the persisted server identity PEM (cert + key). Created on
    /// first startup if missing.
    pub identity_path: PathBuf,
}

impl ServerTunnel {
    /// Build a validated server config. `listen` defaults to `:443`.
    pub fn new(
        listen: Option<String>,
        base_domain: String,
        tokens_file: String,
        identity_path: PathBuf,
    ) -> Result<Self> {
        if base_domain.trim().is_empty() {
            return Err(anyhow!("tunnel.base_domain is required for server mode"));
        }
        if tokens_file.trim().is_empty() {
            return Err(anyhow!("tunnel.tokens_file is required for server mode"));
        }
        if identity_path.as_os_str().is_empty() {
            return Err(anyhow!("tunnel.identity_path is required for server mode"));
        }
        let listen = non_empty(listen).unwrap_or_else(|| ":443".to_string());
        Ok(Self {
            listen,
            base_domain,
            tokens_file,
            identity_path,
        })
    }
}

fn non_empty(v: Option<String>) -> Option<String> {
    v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FP: &str = "0000000000000000000000000000000000000000000000000000000000000000";

    #[test]
    fn client_builds_with_explicit_endpoint() {
        let c = ClientTunnel::new(
            "alice".into(),
            "secret".into(),
            "tunnel.example.com:443".into(),
            FP,
        )
        .unwrap();
        assert_eq!(c.endpoint, "tunnel.example.com:443");
        assert_eq!(c.client_id, "alice");
    }

    #[test]
    fn client_requires_id() {
        assert!(ClientTunnel::new("".into(), "k".into(), "t.example.com:443".into(), FP).is_err());
    }

    #[test]
    fn client_requires_key() {
        assert!(
            ClientTunnel::new("alice".into(), "".into(), "t.example.com:443".into(), FP).is_err()
        );
    }

    #[test]
    fn client_requires_endpoint() {
        assert!(ClientTunnel::new("alice".into(), "k".into(), "".into(), FP).is_err());
    }

    #[test]
    fn client_requires_valid_fingerprint() {
        assert!(
            ClientTunnel::new(
                "alice".into(),
                "k".into(),
                "t.example.com:443".into(),
                "zzzz"
            )
            .is_err()
        );
        assert!(
            ClientTunnel::new("alice".into(), "k".into(), "t.example.com:443".into(), "").is_err()
        );
    }

    #[test]
    fn server_requires_all_fields() {
        let id = PathBuf::from("/tmp/nsl-identity.pem");
        assert!(ServerTunnel::new(None, "".into(), "t".into(), id.clone()).is_err());
        assert!(ServerTunnel::new(None, "d".into(), "".into(), id.clone()).is_err());
        assert!(ServerTunnel::new(None, "d".into(), "t".into(), PathBuf::new()).is_err());
        let s = ServerTunnel::new(None, "d".into(), "t".into(), id).unwrap();
        assert_eq!(s.listen, ":443");
    }

    #[test]
    fn server_listen_override_wins() {
        let s = ServerTunnel::new(
            Some("0.0.0.0:4433".into()),
            "d".into(),
            "t".into(),
            PathBuf::from("/tmp/nsl-identity.pem"),
        )
        .unwrap();
        assert_eq!(s.listen, "0.0.0.0:4433");
    }
}

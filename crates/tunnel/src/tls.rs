//! TLS / QUIC transport setup for the tunnel.
//!
//! Authentication model (no PKI):
//!
//! - Server owns a long-lived self-signed cert persisted to disk. Its
//!   SHA-256(DER) fingerprint is stable across restarts and is the only
//!   piece of server identity the operator needs to publish.
//! - Client pins that fingerprint via [`PubkeyVerifier`]; rustls aborts
//!   the TLS handshake on mismatch, so a MITM fails before the
//!   application layer sees anything.
//! - Client suppresses SNI in the ClientHello so passive DPI can't
//!   fingerprint tunnel traffic by hostname.
//! - The ALPN label is set by `protocol::ALPN` to blend in with HTTP/3
//!   flows on :443/UDP; the bespoke `PROTOCOL_VERSION` gate inside the
//!   Hello frame stops accidental HTTP/3 probes.
//!
//! A second authentication layer — the mutual HMAC handshake in
//! `protocol.rs` — proves tenant identity and double-checks server
//! identity in case the pinned fingerprint is ever rotated out of band.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use sha2::{Digest as _, Sha256};

use crate::protocol::ALPN;

/// Placeholder server_name passed to `Endpoint::connect`. rustls requires
/// a `ServerName` value but the client is configured with `enable_sni =
/// false`, so this value never reaches the wire.
pub const TLS_SERVER_NAME: &str = "nsl";

/// Length of a SHA-256 certificate fingerprint in bytes.
pub const FINGERPRINT_LEN: usize = 32;

/// Load (or lazily create) the server's long-lived self-signed cert and
/// return a QUIC server config plus the SHA-256 fingerprint operators
/// should publish to clients.
///
/// The cert+key are stored as PEM at `identity_path`. The parent
/// directory is created on demand. If the file exists and parses, it is
/// reused verbatim; otherwise a fresh ECDSA P-256 cert is generated,
/// persisted with mode 0600 on Unix, and its fingerprint is returned.
pub fn build_server_crypto(
    identity_path: &Path,
) -> Result<(Arc<QuicServerConfig>, [u8; FINGERPRINT_LEN])> {
    let (cert_der, key_der) = load_or_generate_identity(identity_path)?;
    let fingerprint: [u8; FINGERPRINT_LEN] = Sha256::digest(cert_der.as_ref()).into();

    let mut config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)
        .context("failed to build rustls ServerConfig")?;
    config.alpn_protocols = vec![ALPN.to_vec()];
    // Allow 0-RTT early data so returning clients skip the TLS handshake
    // RTT on reconnect. Replay-safety rests on the application layer:
    // every session proves freshness with a fresh `server_nonce` in the
    // mutual-HMAC handshake, so replayed 0-RTT packets can't forge a
    // session.
    config.max_early_data_size = u32::MAX;

    let quic = QuicServerConfig::try_from(config)
        .context("failed to convert rustls ServerConfig to QuicServerConfig")?;
    Ok((Arc::new(quic), fingerprint))
}

/// Build a QUIC client config that pins `expected_fingerprint` and never
/// sends an SNI extension.
pub fn build_client_crypto(
    expected_fingerprint: [u8; FINGERPRINT_LEN],
) -> Result<Arc<QuicClientConfig>> {
    let mut config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PubkeyVerifier::new(expected_fingerprint)))
        .with_no_client_auth();
    config.alpn_protocols = vec![ALPN.to_vec()];
    config.enable_sni = false;

    let quic = QuicClientConfig::try_from(config)
        .context("failed to convert rustls ClientConfig to QuicClientConfig")?;
    Ok(Arc::new(quic))
}

/// Parse a 64-hex-char fingerprint into a 32-byte array. Accepts optional
/// `sha256:` prefix, colons, and whitespace for operator convenience.
pub fn parse_fingerprint(s: &str) -> Result<[u8; FINGERPRINT_LEN]> {
    let cleaned: String = s
        .trim()
        .trim_start_matches("sha256:")
        .trim_start_matches("SHA256:")
        .chars()
        .filter(|c| !c.is_whitespace() && *c != ':')
        .collect();
    if cleaned.len() != FINGERPRINT_LEN * 2 {
        return Err(anyhow!(
            "fingerprint must be {} hex chars, got {}",
            FINGERPRINT_LEN * 2,
            cleaned.len()
        ));
    }
    let mut out = [0u8; FINGERPRINT_LEN];
    for (i, byte) in out.iter_mut().enumerate() {
        let hex = &cleaned[i * 2..i * 2 + 2];
        *byte = u8::from_str_radix(hex, 16)
            .with_context(|| format!("invalid hex in fingerprint at offset {}", i * 2))?;
    }
    Ok(out)
}

/// Lowercase hex of a fingerprint (no prefix, no separators).
pub fn format_fingerprint(fp: &[u8; FINGERPRINT_LEN]) -> String {
    let mut s = String::with_capacity(FINGERPRINT_LEN * 2);
    for b in fp {
        use std::fmt::Write;
        let _ = write!(s, "{:02x}", b);
    }
    s
}

fn load_or_generate_identity(
    path: &Path,
) -> Result<(CertificateDer<'static>, PrivateKeyDer<'static>)> {
    if path.is_file() {
        return read_identity_pem(path)
            .with_context(|| format!("reading identity from {}", path.display()));
    }
    generate_and_persist_identity(path)
        .with_context(|| format!("generating identity at {}", path.display()))
}

fn read_identity_pem(path: &Path) -> Result<(CertificateDer<'static>, PrivateKeyDer<'static>)> {
    let pem = std::fs::read_to_string(path)?;
    let mut cert: Option<CertificateDer<'static>> = None;
    let mut key: Option<PrivateKeyDer<'static>> = None;
    for block in pem::parse_many(pem.as_bytes())? {
        match block.tag() {
            "CERTIFICATE" => {
                if cert.is_none() {
                    cert = Some(CertificateDer::from(block.contents().to_vec()));
                }
            }
            "PRIVATE KEY" => {
                key = Some(PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
                    block.contents().to_vec(),
                )));
            }
            _ => {}
        }
    }
    match (cert, key) {
        (Some(c), Some(k)) => Ok((c, k)),
        _ => Err(anyhow!(
            "identity file must contain one CERTIFICATE and one PRIVATE KEY block"
        )),
    }
}

fn generate_and_persist_identity(
    path: &Path,
) -> Result<(CertificateDer<'static>, PrivateKeyDer<'static>)> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating identity dir {}", parent.display()))?;
    }
    let issued = rcgen::generate_simple_self_signed(vec![TLS_SERVER_NAME.to_string()])
        .context("generating ephemeral self-signed cert")?;
    let cert_pem = issued.cert.pem();
    let key_pem = issued.signing_key.serialize_pem();
    let mut out = String::with_capacity(cert_pem.len() + key_pem.len());
    out.push_str(&cert_pem);
    out.push_str(&key_pem);
    write_identity_file(path, out.as_bytes())?;

    let cert_der = CertificateDer::from(issued.cert.der().to_vec());
    let key_der = PrivatePkcs8KeyDer::from(issued.signing_key.serialize_der());
    Ok((cert_der, PrivateKeyDer::Pkcs8(key_der)))
}

#[cfg(unix)]
fn write_identity_file(path: &Path, data: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    f.write_all(data)?;
    f.flush()?;
    Ok(())
}

#[cfg(not(unix))]
fn write_identity_file(path: &Path, data: &[u8]) -> Result<()> {
    std::fs::write(path, data).with_context(|| format!("writing {}", path.display()))
}

/// rustls cert verifier that accepts exactly one SHA-256(DER) fingerprint.
#[derive(Debug)]
struct PubkeyVerifier {
    expected: [u8; FINGERPRINT_LEN],
}

impl PubkeyVerifier {
    fn new(expected: [u8; FINGERPRINT_LEN]) -> Self {
        Self { expected }
    }
}

impl ServerCertVerifier for PubkeyVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let got: [u8; FINGERPRINT_LEN] = Sha256::digest(end_entity.as_ref()).into();
        if got == self.expected {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(
                "server certificate fingerprint mismatch".into(),
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpfile_path(name: &str) -> std::path::PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "nsl-tls-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.push(name);
        dir
    }

    #[test]
    fn server_crypto_generates_and_persists_identity() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let path = tmpfile_path("identity.pem");
        let (_cfg1, fp1) = build_server_crypto(&path).unwrap();
        assert!(path.exists());
        let (_cfg2, fp2) = build_server_crypto(&path).unwrap();
        assert_eq!(fp1, fp2, "fingerprint must be stable across reloads");
    }

    #[test]
    fn client_crypto_builds_with_fingerprint() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let _ = build_client_crypto([0u8; FINGERPRINT_LEN]).unwrap();
    }

    #[test]
    fn fingerprint_parse_roundtrip() {
        let fp = [0xdeu8; FINGERPRINT_LEN];
        let s = format_fingerprint(&fp);
        assert_eq!(s.len(), FINGERPRINT_LEN * 2);
        assert_eq!(parse_fingerprint(&s).unwrap(), fp);
    }

    #[test]
    fn fingerprint_accepts_common_formatting() {
        let canonical = "de".repeat(FINGERPRINT_LEN);
        let with_colons = (0..FINGERPRINT_LEN)
            .map(|_| "DE")
            .collect::<Vec<_>>()
            .join(":");
        let with_prefix = format!("sha256:{}", canonical);
        let target = [0xdeu8; FINGERPRINT_LEN];
        assert_eq!(parse_fingerprint(&canonical).unwrap(), target);
        assert_eq!(parse_fingerprint(&with_colons).unwrap(), target);
        assert_eq!(parse_fingerprint(&with_prefix).unwrap(), target);
    }

    #[test]
    fn fingerprint_rejects_wrong_length() {
        assert!(parse_fingerprint("deadbeef").is_err());
    }

    #[test]
    fn pubkey_verifier_rejects_mismatch() {
        let v = PubkeyVerifier::new([0u8; FINGERPRINT_LEN]);
        let cert = CertificateDer::from(b"not-the-right-cert".as_slice());
        let name = ServerName::try_from("nsl").unwrap();
        assert!(
            v.verify_server_cert(&cert, &[], &name, &[], UnixTime::now())
                .is_err()
        );
    }

    #[test]
    fn pubkey_verifier_accepts_matching_fingerprint() {
        let cert_bytes = b"fake-cert-bytes".as_slice();
        let fp: [u8; FINGERPRINT_LEN] = Sha256::digest(cert_bytes).into();
        let v = PubkeyVerifier::new(fp);
        let cert = CertificateDer::from(cert_bytes);
        let name = ServerName::try_from("nsl").unwrap();
        assert!(
            v.verify_server_cert(&cert, &[], &name, &[], UnixTime::now())
                .is_ok()
        );
    }
}

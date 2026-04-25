mod generate;
mod sni;
mod trust;

pub use generate::{generate_ca, generate_host_cert, generate_server_cert};
pub use sni::SniCertResolver;
pub use trust::{TrustResult, trust_ca};

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rcgen::{Issuer, KeyPair};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::sign::CertifiedKey;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// CA certificate validity in days (10 years).
pub(super) const CA_VALIDITY_DAYS: u32 = 3650;

/// Server certificate validity in days.
pub(super) const SERVER_VALIDITY_DAYS: u32 = 365;

/// Regenerate certificates when they expire within this many days.
const EXPIRY_BUFFER_DAYS: u32 = 7;

/// Common name used for the local CA certificate.
pub(super) const CA_COMMON_NAME: &str = "nsl Local CA";

// File names inside the state directory.
pub(super) const CA_KEY_FILE: &str = "ca-key.pem";
pub(super) const CA_CERT_FILE: &str = "ca.pem";
pub(super) const SERVER_KEY_FILE: &str = "server-key.pem";
pub(super) const SERVER_CERT_FILE: &str = "server.pem";

/// Sub-directory for per-hostname certificates.
pub(super) const HOST_CERTS_DIR: &str = "host-certs";

// ---------------------------------------------------------------------------
// Public result types
// ---------------------------------------------------------------------------

/// Paths to the CA and server cert/key files.
#[derive(Debug, Clone)]
pub struct CertPaths {
    pub ca_cert: PathBuf,
    #[allow(dead_code)]
    pub ca_key: PathBuf,
    pub server_cert: PathBuf,
    pub server_key: PathBuf,
}

// ---------------------------------------------------------------------------
// Certificate validation
// ---------------------------------------------------------------------------

/// Check whether the certificate at `cert_path` is still valid, accounting
/// for an expiry buffer of `EXPIRY_BUFFER_DAYS`.
pub fn is_cert_valid(cert_path: &Path) -> bool {
    let pem_data = match fs::read(cert_path) {
        Ok(d) => d,
        Err(_) => return false,
    };

    let (_, pem) = match x509_parser::pem::parse_x509_pem(&pem_data) {
        Ok(result) => result,
        Err(_) => return false,
    };
    let cert = match pem.parse_x509() {
        Ok(c) => c,
        Err(_) => return false,
    };

    let not_after = cert.validity().not_after.to_datetime();
    let buffer = time::Duration::days(EXPIRY_BUFFER_DAYS as i64);
    let threshold = match time::OffsetDateTime::now_utc().checked_add(buffer) {
        Some(t) => t,
        None => return false,
    };

    not_after > threshold
}

/// Ensure that both CA and server certificates exist and are valid.
/// Generates missing or expired certificates as needed.
pub fn ensure_certs(state_dir: &Path) -> anyhow::Result<CertPaths> {
    fs::create_dir_all(state_dir)?;

    let ca_cert_path = state_dir.join(CA_CERT_FILE);
    let ca_key_path = state_dir.join(CA_KEY_FILE);
    let server_cert_path = state_dir.join(SERVER_CERT_FILE);
    let server_key_path = state_dir.join(SERVER_KEY_FILE);

    let mut ca_regenerated = false;

    if !ca_cert_path.exists() || !ca_key_path.exists() || !is_cert_valid(&ca_cert_path) {
        generate_ca(state_dir)?;
        ca_regenerated = true;
    }

    if ca_regenerated
        || !server_cert_path.exists()
        || !server_key_path.exists()
        || !is_cert_valid(&server_cert_path)
    {
        generate_server_cert(state_dir)?;
    }

    Ok(CertPaths {
        ca_cert: ca_cert_path,
        ca_key: ca_key_path,
        server_cert: server_cert_path,
        server_key: server_key_path,
    })
}

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

/// Load the CA signer from PEM files on disk.
pub(super) fn load_ca_keypair(state_dir: &Path) -> anyhow::Result<Issuer<'static, KeyPair>> {
    let key_pem = fs::read_to_string(state_dir.join(CA_KEY_FILE))?;
    let ca_key_pair = KeyPair::from_pem(&key_pem)?;

    let cert_pem = fs::read_to_string(state_dir.join(CA_CERT_FILE))?;
    let issuer = Issuer::from_ca_cert_pem(&cert_pem, ca_key_pair)?;

    Ok(issuer)
}

/// Load a certified key (cert chain + signing key) from PEM files.
pub(super) fn load_certified_key(
    cert_path: &Path,
    key_path: &Path,
) -> anyhow::Result<CertifiedKey> {
    install_ring_provider_once();

    let cert_pem = fs::read(cert_path)?;
    let key_pem = fs::read(key_path)?;

    let certs: Vec<CertificateDer<'static>> =
        rustls_pemfile::certs(&mut &cert_pem[..]).collect::<Result<Vec<_>, _>>()?;

    let key_der: PrivateKeyDer<'static> = rustls_pemfile::private_key(&mut &key_pem[..])?
        .ok_or_else(|| anyhow::anyhow!("no private key found in {}", key_path.display()))?;

    let signing_key = rustls::crypto::ring::sign::any_ecdsa_type(&key_der)?;

    Ok(CertifiedKey::new(certs, signing_key))
}

/// Assert that a constructed path stays within the expected parent directory.
pub(super) fn assert_path_containment(child: &Path, parent: &Path) -> anyhow::Result<()> {
    let canonical_parent = parent
        .canonicalize()
        .unwrap_or_else(|_| parent.to_path_buf());
    let canonical_child = child.canonicalize().unwrap_or_else(|_| child.to_path_buf());
    anyhow::ensure!(
        canonical_child.starts_with(&canonical_parent),
        "path traversal detected: {} is outside {}",
        child.display(),
        parent.display()
    );
    Ok(())
}

/// Domains considered local for wildcard SAN generation.
const LOCAL_DOMAINS: &[&str] = &["localhost", "local", "test", "internal", "dev"];

/// Check if a domain suffix is a known local domain.
pub(super) fn is_local_domain(domain: &str) -> bool {
    let lower = domain.to_ascii_lowercase();
    LOCAL_DOMAINS
        .iter()
        .any(|d| lower == *d || lower.ends_with(&format!(".{}", d)))
}

/// Set UNIX file permissions (no-op on non-Unix platforms).
pub(super) fn set_file_mode(path: &Path, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = fs::set_permissions(path, fs::Permissions::from_mode(mode)) {
            tracing::warn!(
                "failed to set permissions {:o} on {}: {}",
                mode,
                path.display(),
                e
            );
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
}

/// Write a private key file with restricted permissions (0o600) atomically.
pub(super) fn write_key_file(path: &Path, pem_data: &str) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(pem_data.as_bytes())?;
    }
    #[cfg(not(unix))]
    {
        fs::write(path, pem_data)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// TLS server config builder
// ---------------------------------------------------------------------------

/// Build a `rustls::ServerConfig` for the proxy, using the SNI resolver.
pub fn build_tls_server_config(
    cert_paths: &CertPaths,
    state_dir: PathBuf,
) -> anyhow::Result<rustls::ServerConfig> {
    install_ring_provider_once();

    let resolver =
        SniCertResolver::new(&cert_paths.server_cert, &cert_paths.server_key, state_dir)?;

    let mut config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(resolver));

    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];

    Ok(config)
}

fn install_ring_provider_once() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_is_local_domain() {
        assert!(is_local_domain("localhost"));
        assert!(is_local_domain("local"));
        assert!(is_local_domain("test"));
        assert!(is_local_domain("dev"));
        assert!(is_local_domain("internal"));
        assert!(!is_local_domain("example.com"));
        assert!(!is_local_domain("com"));
    }

    #[test]
    fn test_is_cert_valid_with_valid_cert() {
        let tmp = TempDir::new().unwrap();
        let (cert_path, _) = generate_ca(tmp.path()).unwrap();
        assert!(
            is_cert_valid(&cert_path),
            "freshly generated cert should be valid"
        );
    }

    #[test]
    fn test_is_cert_valid_missing_file() {
        assert!(!is_cert_valid(Path::new("/nonexistent/cert.pem")));
    }

    #[test]
    fn test_is_cert_valid_invalid_pem() {
        let tmp = TempDir::new().unwrap();
        let bad_path = tmp.path().join("bad.pem");
        fs::write(&bad_path, "not a certificate").unwrap();
        assert!(!is_cert_valid(&bad_path));
    }

    #[test]
    fn test_ensure_certs_generates_when_missing() {
        let tmp = TempDir::new().unwrap();
        let paths = ensure_certs(tmp.path()).unwrap();

        assert!(paths.ca_cert.exists());
        assert!(paths.ca_key.exists());
        assert!(paths.server_cert.exists());
        assert!(paths.server_key.exists());
    }

    #[test]
    fn test_ensure_certs_idempotent() {
        let tmp = TempDir::new().unwrap();

        let paths1 = ensure_certs(tmp.path()).unwrap();
        let cert1 = fs::read(&paths1.server_cert).unwrap();

        let paths2 = ensure_certs(tmp.path()).unwrap();
        let cert2 = fs::read(&paths2.server_cert).unwrap();

        assert_eq!(cert1, cert2, "cert should not change when still valid");
    }

    #[test]
    fn test_ensure_certs_regenerates_when_ca_missing() {
        let tmp = TempDir::new().unwrap();
        ensure_certs(tmp.path()).unwrap();

        fs::remove_file(tmp.path().join(CA_KEY_FILE)).unwrap();
        let paths = ensure_certs(tmp.path()).unwrap();
        assert!(paths.ca_key.exists(), "CA key should be regenerated");
    }

    #[test]
    fn test_build_tls_server_config() {
        let tmp = TempDir::new().unwrap();
        let paths = ensure_certs(tmp.path()).unwrap();
        let config = build_tls_server_config(&paths, tmp.path().to_path_buf());
        assert!(
            config.is_ok(),
            "should build TLS server config without error"
        );
    }

    #[test]
    fn test_load_certified_key() {
        let tmp = TempDir::new().unwrap();
        ensure_certs(tmp.path()).unwrap();

        let result = load_certified_key(
            &tmp.path().join(SERVER_CERT_FILE),
            &tmp.path().join(SERVER_KEY_FILE),
        );
        assert!(result.is_ok(), "should load certified key from PEM files");
    }
}

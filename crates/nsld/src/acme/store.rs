//! On-disk persistence for ACME account credentials and issued certs.
//!
//! Layout (under `state_dir/acme/`):
//!
//! ```text
//! account.key                              # JSON credentials blob from instant-acme
//! certs/<tenant.apex.domain>.pem          # CERTIFICATE chain + PRIVATE KEY
//! ```

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

/// Directory layout helper: everything the ACME manager needs to know
/// about disk paths comes through here.
#[derive(Debug, Clone)]
pub struct AcmeStoreLayout {
    pub root: PathBuf,
}

impl AcmeStoreLayout {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn account_file(&self) -> PathBuf {
        self.root.join("account.key")
    }

    pub fn certs_dir(&self) -> PathBuf {
        self.root.join("certs")
    }

    pub fn cert_file(&self, tenant: &str) -> PathBuf {
        self.certs_dir().join(format!("{}.pem", tenant))
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        std::fs::create_dir_all(self.certs_dir())
            .with_context(|| format!("creating {}", self.certs_dir().display()))?;
        Ok(())
    }
}

/// Load ACME account credentials blob if present.
pub fn load_account_credentials(path: &Path) -> Result<Option<String>> {
    if !path.is_file() {
        return Ok(None);
    }
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(Some(raw))
}

/// Persist ACME account credentials (atomic write + mode 0600 on unix).
pub fn save_account_credentials(path: &Path, credentials_json: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    write_private(path, credentials_json.as_bytes())
}

/// Result of loading a persisted cert: the chain + its PKCS8 private key,
/// ready to be turned into a `rustls::sign::CertifiedKey`.
pub struct StoredCert {
    pub chain: Vec<CertificateDer<'static>>,
    pub key: PrivateKeyDer<'static>,
    /// Raw DER of the end-entity cert — callers use this for expiry
    /// inspection.
    pub end_entity_der: Vec<u8>,
}

/// Load a previously-issued cert for `tenant` from disk.
pub fn load_cert(layout: &AcmeStoreLayout, tenant: &str) -> Result<Option<StoredCert>> {
    let path = layout.cert_file(tenant);
    if !path.is_file() {
        return Ok(None);
    }
    let pem_bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    let mut chain = Vec::new();
    let mut key: Option<PrivateKeyDer<'static>> = None;
    for block in pem::parse_many(&pem_bytes)? {
        match block.tag() {
            "CERTIFICATE" => chain.push(CertificateDer::from(block.contents().to_vec())),
            "PRIVATE KEY" => {
                key = Some(PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
                    block.contents().to_vec(),
                )));
            }
            _ => {}
        }
    }
    let Some(key) = key else {
        anyhow::bail!("{} missing PRIVATE KEY block", path.display());
    };
    if chain.is_empty() {
        anyhow::bail!("{} has no CERTIFICATE blocks", path.display());
    }
    let end_entity_der = chain[0].as_ref().to_vec();
    Ok(Some(StoredCert {
        chain,
        key,
        end_entity_der,
    }))
}

/// Persist a newly-issued cert as PEM (chain + key).
pub fn save_cert(
    layout: &AcmeStoreLayout,
    tenant: &str,
    chain: &[CertificateDer<'static>],
    key_pkcs8_der: &[u8],
) -> Result<()> {
    layout.ensure_dirs()?;
    let mut out = String::new();
    for cert in chain {
        out.push_str(&pem::encode(&pem::Pem::new(
            "CERTIFICATE",
            cert.as_ref().to_vec(),
        )));
    }
    out.push_str(&pem::encode(&pem::Pem::new(
        "PRIVATE KEY",
        key_pkcs8_der.to_vec(),
    )));
    let path = layout.cert_file(tenant);
    write_private(&path, out.as_bytes())
}

#[cfg(unix)]
fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // `create_new` + rename provides atomicity without clobbering a
    // concurrent reader partway through a write.
    let tmp = path.with_extension("tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("creating {}", tmp.display()))?;
    f.write_all(data)?;
    f.flush()?;
    drop(f);
    std::fs::rename(&tmp, path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, data).with_context(|| format!("writing {}", path.display()))
}

/// Parse the `not_after` from an X.509 DER blob as a SystemTime.
pub fn cert_expiry(der: &[u8]) -> Result<std::time::SystemTime> {
    let (_, cert) = x509_parser::parse_x509_certificate(der)
        .map_err(|e| anyhow::anyhow!("x509 parse: {}", e))?;
    let not_after = cert.validity().not_after.timestamp();
    if not_after < 0 {
        anyhow::bail!("cert not_after is pre-epoch");
    }
    Ok(std::time::UNIX_EPOCH + std::time::Duration::from_secs(not_after as u64))
}

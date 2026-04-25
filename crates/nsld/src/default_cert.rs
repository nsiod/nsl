//! Private-CA-backed self-signed certs for the HTTPS edge.
//!
//! Runs alongside the ACME manager (never in place of it) and acts as
//! a fallback whenever the ACME path hasn't produced a trusted cert
//! yet — e.g. while an initial issuance is in flight, or indefinitely
//! when DNS is misconfigured. The resolver always prefers the ACME
//! entry in the shared cache; this manager only writes to a tenant
//! slot if it is empty (`entry().or_insert_with`), and ACME's
//! successful issuance unconditionally overwrites with `insert`.
//!
//! On first startup a long-lived self-signed CA is generated at
//! `state_dir/default-ca.pem` (cert + key). Subsequent startups reuse
//! it. For each tenant the daemon sees come online, the manager
//! lazily issues a leaf cert (SAN: `apex` + `*.apex`) **signed by that
//! CA** and persists it at `state_dir/default-certs/<tenant>.pem`.
//!
//! Operator workflow for a trusted dev environment:
//!   1. Start nsld once; note the CA fingerprint printed at startup.
//!   2. Import `state_dir/default-ca.pem` into the OS / browser trust
//!      store.
//!   3. All tenant leaf certs now chain cleanly — no browser warnings.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::sign::CertifiedKey;
use sha2::{Digest as _, Sha256};

use crate::acme::CertCache;

const CA_COMMON_NAME: &str = "nsld default CA";
const CA_FILE: &str = "default-ca.pem";
const CERTS_SUBDIR: &str = "default-certs";

#[derive(Clone)]
pub struct DefaultCertManager {
    inner: Arc<Inner>,
}

struct Inner {
    state_dir: PathBuf,
    cache: CertCache,
    ca: Ca,
}

/// The live CA we sign leaves with. The `issuer` is the rcgen handle
/// used at signing time; `cert_der` is kept separately so we can attach
/// it to every leaf chain we publish.
struct Ca {
    issuer: rcgen::Issuer<'static, rcgen::KeyPair>,
    cert_der: CertificateDer<'static>,
    cert_fingerprint: [u8; 32],
}

impl DefaultCertManager {
    pub fn start(state_dir: PathBuf, cache: CertCache) -> Result<Self> {
        std::fs::create_dir_all(&state_dir)
            .with_context(|| format!("creating {}", state_dir.display()))?;
        std::fs::create_dir_all(state_dir.join(CERTS_SUBDIR))?;
        let ca = load_or_generate_ca(&state_dir.join(CA_FILE))?;
        let fingerprint_hex = fingerprint_to_hex(&ca.cert_fingerprint);
        tracing::info!(
            path = %state_dir.join(CA_FILE).display(),
            fingerprint = %fingerprint_hex,
            "default self-signed CA ready"
        );
        println!(
            "default CA sha256 fingerprint: {}   (import {} into your trust store to silence browser warnings)",
            fingerprint_hex,
            state_dir.join(CA_FILE).display()
        );

        let inner = Arc::new(Inner {
            state_dir,
            cache,
            ca,
        });
        hydrate_from_disk(&inner)?;
        Ok(Self { inner })
    }

    /// Non-blocking trigger — same interface as [`crate::acme::AcmeManager::ensure_cert`].
    pub fn ensure_cert(&self, tenant: &str) {
        let tenant = tenant.to_ascii_lowercase();
        let already = self
            .inner
            .cache
            .read()
            .map(|g| g.contains_key(&tenant))
            .unwrap_or(false);
        if already {
            return;
        }
        let mgr = self.clone();
        tokio::task::spawn_blocking(move || {
            if let Err(e) = mgr.issue(&tenant) {
                tracing::warn!(tenant, error = %e, "default CA leaf issuance failed");
            }
        });
    }

    fn issue(&self, tenant: &str) -> Result<()> {
        if self
            .inner
            .cache
            .read()
            .map(|g| g.contains_key(tenant))
            .unwrap_or(false)
        {
            return Ok(());
        }

        let path = self.leaf_path(tenant);
        let (chain, key_pkcs8_der) = if path.is_file() {
            load_leaf_pem(&path)?
        } else {
            self.sign_new_leaf(tenant, &path)?
        };

        let signer_key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_pkcs8_der));
        let signer = rustls::crypto::ring::sign::any_supported_type(&signer_key)
            .map_err(|e| anyhow!("building signer: {}", e))?;
        let certified = Arc::new(CertifiedKey::new(chain, signer));

        // `or_insert_with` so we never clobber an ACME-issued cert that
        // raced ahead of us into the slot.
        let mut cache = self
            .inner
            .cache
            .write()
            .map_err(|_| anyhow!("cache poisoned"))?;
        let inserted = !cache.contains_key(tenant);
        cache.entry(tenant.to_string()).or_insert(certified);
        drop(cache);
        if inserted {
            tracing::info!(tenant, "default CA-signed leaf installed as fallback");
        } else {
            tracing::debug!(
                tenant,
                "default CA-signed leaf on disk but ACME cert already in cache; keeping ACME"
            );
        }
        Ok(())
    }

    fn sign_new_leaf(
        &self,
        tenant: &str,
        path: &Path,
    ) -> Result<(Vec<CertificateDer<'static>>, Vec<u8>)> {
        let san = vec![tenant.to_string(), format!("*.{}", tenant)];
        let mut params =
            rcgen::CertificateParams::new(san).context("leaf CertificateParams::new")?;
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, tenant);

        let leaf_key = rcgen::KeyPair::generate().context("leaf KeyPair::generate")?;
        let leaf = params
            .signed_by(&leaf_key, &self.inner.ca.issuer)
            .context("signing leaf with default CA")?;

        let leaf_der = leaf.der().to_vec();
        let leaf_key_der = leaf_key.serialize_der();
        // Chain: [leaf, CA] so clients that don't have the CA in their
        // trust store still see the signing authority during the
        // handshake (they'll still reject it as untrusted, but tooling
        // like `openssl s_client` will display the chain correctly).
        write_leaf_pem(
            path,
            &leaf_der,
            self.inner.ca.cert_der.as_ref(),
            &leaf_key_der,
        )?;

        let chain = vec![
            CertificateDer::from(leaf_der),
            self.inner.ca.cert_der.clone(),
        ];
        Ok((chain, leaf_key_der))
    }

    fn leaf_path(&self, tenant: &str) -> PathBuf {
        self.inner
            .state_dir
            .join(CERTS_SUBDIR)
            .join(format!("{}.pem", tenant))
    }
}

fn load_or_generate_ca(path: &Path) -> Result<Ca> {
    if path.is_file() {
        return read_ca(path).with_context(|| format!("reading CA at {}", path.display()));
    }
    generate_and_persist_ca(path).with_context(|| format!("generating CA at {}", path.display()))
}

fn read_ca(path: &Path) -> Result<Ca> {
    let raw = std::fs::read_to_string(path)?;
    let mut cert_pem: Option<Vec<u8>> = None;
    let mut key_pem: Option<String> = None;
    for block in pem::parse_many(raw.as_bytes())? {
        match block.tag() {
            "CERTIFICATE" if cert_pem.is_none() => {
                cert_pem = Some(block.contents().to_vec());
            }
            "PRIVATE KEY" if key_pem.is_none() => {
                key_pem = Some(pem::encode(&block));
            }
            _ => {}
        }
    }
    let (cert_contents, key_pem) = match (cert_pem, key_pem) {
        (Some(c), Some(k)) => (c, k),
        _ => anyhow::bail!("CA file missing CERTIFICATE or PRIVATE KEY block"),
    };
    let signing_key = rcgen::KeyPair::from_pem(&key_pem).context("parsing CA key")?;
    let cert_der = CertificateDer::from(cert_contents.clone());
    let issuer = rcgen::Issuer::from_ca_cert_der(&cert_der, signing_key)
        .context("building Issuer from CA cert")?;
    let fingerprint: [u8; 32] = Sha256::digest(&cert_contents).into();
    Ok(Ca {
        issuer,
        cert_der: CertificateDer::from(cert_contents),
        cert_fingerprint: fingerprint,
    })
}

fn generate_and_persist_ca(path: &Path) -> Result<Ca> {
    let mut params =
        rcgen::CertificateParams::new(Vec::<String>::new()).context("CA CertificateParams::new")?;
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, CA_COMMON_NAME);
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    params.key_usages = vec![
        rcgen::KeyUsagePurpose::KeyCertSign,
        rcgen::KeyUsagePurpose::CrlSign,
        rcgen::KeyUsagePurpose::DigitalSignature,
    ];

    let key = rcgen::KeyPair::generate().context("CA KeyPair::generate")?;
    let ca_cert = params.self_signed(&key).context("self-sign CA")?;

    let cert_pem = ca_cert.pem();
    let key_pem = key.serialize_pem();
    let combined = format!("{cert_pem}{key_pem}");
    write_private(path, combined.as_bytes())?;

    let cert_der_vec = ca_cert.der().to_vec();
    let fingerprint: [u8; 32] = Sha256::digest(&cert_der_vec).into();
    let issuer = rcgen::Issuer::new(params, key);
    Ok(Ca {
        issuer,
        cert_der: CertificateDer::from(cert_der_vec),
        cert_fingerprint: fingerprint,
    })
}

fn hydrate_from_disk(inner: &Inner) -> Result<()> {
    let dir = inner.state_dir.join(CERTS_SUBDIR);
    if !dir.is_dir() {
        return Ok(());
    }
    let mut cache = inner.cache.write().map_err(|_| anyhow!("cache poisoned"))?;
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(tenant) = name.strip_suffix(".pem") else {
            continue;
        };
        match load_leaf_pem(&path) {
            Ok((chain, key_der)) => {
                let signer_key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_der));
                let Ok(signer) = rustls::crypto::ring::sign::any_supported_type(&signer_key) else {
                    tracing::warn!(tenant, "bad signing key in {}", path.display());
                    continue;
                };
                // Defer to any ACME cert that was hydrated before us.
                let already = cache.contains_key(tenant);
                cache
                    .entry(tenant.to_string())
                    .or_insert_with(|| Arc::new(CertifiedKey::new(chain, signer)));
                if !already {
                    tracing::info!(tenant, "hydrated default CA leaf from disk");
                }
            }
            Err(e) => tracing::warn!(tenant, error = %e, "load default leaf"),
        }
    }
    Ok(())
}

fn load_leaf_pem(path: &Path) -> Result<(Vec<CertificateDer<'static>>, Vec<u8>)> {
    let pem_bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let mut chain = Vec::new();
    let mut key_der: Option<Vec<u8>> = None;
    for block in pem::parse_many(&pem_bytes)? {
        match block.tag() {
            "CERTIFICATE" => chain.push(CertificateDer::from(block.contents().to_vec())),
            "PRIVATE KEY" => key_der = Some(block.contents().to_vec()),
            _ => {}
        }
    }
    let key_der = key_der.ok_or_else(|| anyhow!("{} missing PRIVATE KEY", path.display()))?;
    if chain.is_empty() {
        anyhow::bail!("{} has no CERTIFICATE blocks", path.display());
    }
    Ok((chain, key_der))
}

fn write_leaf_pem(
    path: &Path,
    leaf_der: &[u8],
    ca_der: &[u8],
    leaf_key_pkcs8_der: &[u8],
) -> Result<()> {
    let mut out = String::new();
    out.push_str(&pem::encode(&pem::Pem::new(
        "CERTIFICATE",
        leaf_der.to_vec(),
    )));
    out.push_str(&pem::encode(&pem::Pem::new("CERTIFICATE", ca_der.to_vec())));
    out.push_str(&pem::encode(&pem::Pem::new(
        "PRIVATE KEY",
        leaf_key_pkcs8_der.to_vec(),
    )));
    write_private(path, out.as_bytes())
}

fn fingerprint_to_hex(fp: &[u8; 32]) -> String {
    let mut s = String::with_capacity(64);
    for b in fp {
        use std::fmt::Write;
        let _ = write!(s, "{:02x}", b);
    }
    s
}

#[cfg(unix)]
fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acme::new_cache;

    fn tmp_dir() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "nsld-default-ca-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        p
    }

    #[tokio::test]
    async fn ca_is_stable_across_restarts() {
        let dir = tmp_dir();
        let mgr1 = DefaultCertManager::start(dir.clone(), new_cache()).unwrap();
        let ca_fp_1 = mgr1.inner.ca.cert_fingerprint;

        let mgr2 = DefaultCertManager::start(dir.clone(), new_cache()).unwrap();
        let ca_fp_2 = mgr2.inner.ca.cert_fingerprint;

        assert_eq!(
            ca_fp_1, ca_fp_2,
            "CA must be reused (not regenerated) across restarts"
        );
    }

    #[tokio::test]
    async fn leaf_chains_to_ca() {
        let dir = tmp_dir();
        let cache = new_cache();
        let mgr = DefaultCertManager::start(dir, cache.clone()).unwrap();
        mgr.issue("alice.example.com").unwrap();

        let guard = cache.read().unwrap();
        let ck = guard.get("alice.example.com").unwrap();
        assert!(ck.cert.len() >= 2, "chain must include leaf + CA");
        // Last entry in the chain must match our CA's DER.
        let ca_der_in_chain = ck.cert.last().unwrap().as_ref();
        assert_eq!(ca_der_in_chain, mgr.inner.ca.cert_der.as_ref());
    }

    #[tokio::test]
    async fn default_issue_does_not_overwrite_existing_cache_entry() {
        // Simulate an ACME cert that beat the default into the cache.
        let dir = tmp_dir();
        let cache = new_cache();
        let mgr = DefaultCertManager::start(dir, cache.clone()).unwrap();

        // Inject a fake ACME cert by signing via the default CA (shape
        // doesn't matter — the test only watches Arc identity to
        // confirm the default manager doesn't replace it).
        let fake_acme_chain = {
            let mut p = rcgen::CertificateParams::new(vec!["alice.example.com".into()]).unwrap();
            p.distinguished_name
                .push(rcgen::DnType::CommonName, "alice.example.com");
            let k = rcgen::KeyPair::generate().unwrap();
            let c = p.signed_by(&k, &mgr.inner.ca.issuer).unwrap();
            let der = c.der().to_vec();
            let key_der = k.serialize_der();
            let signer_key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_der));
            let signer = rustls::crypto::ring::sign::any_supported_type(&signer_key).unwrap();
            Arc::new(CertifiedKey::new(vec![CertificateDer::from(der)], signer))
        };
        cache
            .write()
            .unwrap()
            .insert("alice.example.com".into(), fake_acme_chain.clone());

        // Now ask the default manager to ensure cert. It should find
        // the slot occupied and leave it alone.
        mgr.issue("alice.example.com").unwrap();

        let after = cache
            .read()
            .unwrap()
            .get("alice.example.com")
            .cloned()
            .unwrap();
        assert!(
            Arc::ptr_eq(&fake_acme_chain, &after),
            "default manager must not overwrite an existing cache entry"
        );
    }

    #[tokio::test]
    async fn leaf_is_stable_across_restarts() {
        let dir = tmp_dir();
        let mgr1 = DefaultCertManager::start(dir.clone(), new_cache()).unwrap();
        mgr1.issue("alice.example.com").unwrap();
        let first_leaf = mgr1
            .inner
            .cache
            .read()
            .unwrap()
            .get("alice.example.com")
            .unwrap()
            .cert[0]
            .as_ref()
            .to_vec();

        let cache2 = new_cache();
        let _mgr2 = DefaultCertManager::start(dir, cache2.clone()).unwrap();
        let second_leaf = cache2
            .read()
            .unwrap()
            .get("alice.example.com")
            .unwrap()
            .cert[0]
            .as_ref()
            .to_vec();
        assert_eq!(first_leaf, second_leaf);
    }
}

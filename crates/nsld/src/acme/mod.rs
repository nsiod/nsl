//! Per-tenant ACME (Let's Encrypt) issuance for `*.{tenant}.{base_domain}`.
//!
//! Design in a nutshell:
//!
//! * One persisted ACME account shared across all tenants
//!   (`state_dir/acme/account.key`).
//! * For each tenant the daemon sees come online, we ensure a valid
//!   wildcard cert covering both the apex (`alice.nsl.example.com`) and
//!   the wildcard (`*.alice.nsl.example.com`). Issuance is proactive:
//!   triggered on the `SessionHook` fired after a successful tunnel
//!   handshake, so the cert is ready before the first public HTTPS
//!   request arrives.
//! * DNS-01 challenges are fulfilled by POSTing to an operator-supplied
//!   webhook (`httpreq` convention, see [`httpreq`]).
//! * A background renewal task re-checks every 12 hours and re-issues
//!   any cert with <30 days remaining.
//!
//! Concurrency: at most one in-flight issuance per tenant. Duplicate
//! triggers are coalesced onto the existing job.

pub mod httpreq;
pub mod resolver;
pub mod store;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, anyhow};
use instant_acme::{
    Account, AccountCredentials, AuthorizationStatus, ChallengeType, Identifier, NewAccount,
    NewOrder, OrderStatus, RetryPolicy,
};
use rustls::pki_types::PrivateKeyDer;
use rustls::sign::CertifiedKey;
use tokio::sync::{Mutex, RwLock};
use tokio_util::sync::CancellationToken;

pub use resolver::{AcmeResolver, CertCache, new_cache};
pub use store::AcmeStoreLayout;

use self::httpreq::HttpreqClient;

const RENEW_CHECK_INTERVAL: Duration = Duration::from_secs(12 * 3600);

#[derive(Debug, Clone)]
pub struct AcmeConfig {
    pub enable: bool,
    pub contact_email: String,
    pub directory: String,
    pub httpreq_url: String,
    /// HTTP Basic Auth username for the httpreq webhook. Paired with
    /// `httpreq_password`; either both set or both unset.
    pub httpreq_username: Option<String>,
    pub httpreq_password: Option<String>,
    pub propagation_wait_secs: u64,
    pub renewal_threshold_days: i64,
    pub store_root: PathBuf,
}

impl AcmeConfig {
    pub fn validate(&self) -> Result<()> {
        if !self.enable {
            return Ok(());
        }
        if self.contact_email.trim().is_empty() {
            return Err(anyhow!("acme.contact_email is required when acme.enable"));
        }
        if self.directory.trim().is_empty() {
            return Err(anyhow!("acme.directory is required when acme.enable"));
        }
        if self.httpreq_url.trim().is_empty() {
            return Err(anyhow!("acme.httpreq_url is required when acme.enable"));
        }
        // Basic Auth must be all-or-nothing so operators don't end up
        // silently unauthenticated because one half of the pair was
        // mistyped.
        match (
            self.httpreq_username.as_deref().map(str::trim),
            self.httpreq_password.as_deref().map(str::trim),
        ) {
            (Some(""), _) | (_, Some("")) | (Some(_), None) | (None, Some(_)) => {
                return Err(anyhow!(
                    "acme.httpreq_username and acme.httpreq_password must be set together"
                ));
            }
            _ => {}
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct AcmeManager {
    inner: Arc<AcmeInner>,
}

struct AcmeInner {
    config: AcmeConfig,
    layout: AcmeStoreLayout,
    account: Account,
    httpreq: HttpreqClient,
    cache: CertCache,
    /// Tenants with an in-flight `ensure_cert` — keeps concurrent callers
    /// from firing duplicate orders.
    in_flight: Mutex<HashSet<String>>,
    /// Tenants we've seen online; used by the renewal loop so we don't
    /// try to renew a tenant who disappeared.
    known_tenants: RwLock<HashSet<String>>,
}

impl AcmeManager {
    /// Build a manager, creating or reusing the ACME account on disk.
    /// Pre-populates the cert cache from any certs already persisted.
    pub async fn start(config: AcmeConfig, cache: CertCache) -> Result<Self> {
        config.validate()?;
        let layout = AcmeStoreLayout::new(config.store_root.clone());
        layout.ensure_dirs()?;

        let account = load_or_create_account(&layout, &config)
            .await
            .context("initializing ACME account")?;
        let httpreq = HttpreqClient::new(
            &config.httpreq_url,
            config.httpreq_username.clone(),
            config.httpreq_password.clone(),
        )?;

        let inner = Arc::new(AcmeInner {
            config,
            layout,
            account,
            httpreq,
            cache,
            in_flight: Mutex::new(HashSet::new()),
            known_tenants: RwLock::new(HashSet::new()),
        });

        // Hydrate cache from any already-persisted certs in certs_dir.
        hydrate_cache_from_disk(&inner)?;

        Ok(Self { inner })
    }

    /// Spawn the renewal background loop. Must be called once. Returns
    /// the join handle; dropping it stops renewal.
    pub fn spawn_renewal(&self, cancel: CancellationToken) -> tokio::task::JoinHandle<()> {
        let mgr = self.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(RENEW_CHECK_INTERVAL);
            loop {
                tokio::select! {
                    _ = ticker.tick() => {
                        if let Err(e) = mgr.run_renewal_pass().await {
                            tracing::warn!(error = %e, "ACME renewal pass error");
                        }
                    }
                    _ = cancel.cancelled() => return,
                }
            }
        })
    }

    /// Non-blocking trigger. If the tenant already has a valid cached
    /// cert, this returns immediately. Otherwise the caller-side task
    /// spawns issuance in the background and returns at once.
    pub fn ensure_cert(&self, tenant: &str) {
        let tenant = tenant.to_ascii_lowercase();
        let mgr = self.clone();
        tokio::spawn(async move {
            if let Err(e) = mgr.ensure_cert_inner(&tenant).await {
                tracing::warn!(tenant = %tenant, error = %e, "ACME ensure_cert failed");
            }
        });
    }

    async fn ensure_cert_inner(&self, tenant: &str) -> Result<()> {
        self.inner
            .known_tenants
            .write()
            .await
            .insert(tenant.to_string());

        if self.cache_valid(tenant) {
            return Ok(());
        }

        // Single-flight lock for this tenant.
        {
            let mut guard = self.inner.in_flight.lock().await;
            if !guard.insert(tenant.to_string()) {
                tracing::debug!(tenant, "issuance already in flight, skipping duplicate");
                return Ok(());
            }
        }
        let result = self.issue(tenant).await;
        self.inner.in_flight.lock().await.remove(tenant);
        result
    }

    fn cache_valid(&self, tenant: &str) -> bool {
        // Quick path: if in cache and parsed expiry is > threshold days
        // away, reuse. We don't store expiry separately; re-parse as
        // needed from the stored cert on disk.
        let Ok(Some(stored)) = store::load_cert(&self.inner.layout, tenant) else {
            return false;
        };
        let Ok(expiry) = store::cert_expiry(&stored.end_entity_der) else {
            return false;
        };
        let threshold =
            Duration::from_secs((self.inner.config.renewal_threshold_days.max(1) as u64) * 86400);
        let Ok(remaining) = expiry.duration_since(SystemTime::now()) else {
            return false;
        };
        if remaining < threshold {
            return false;
        }
        // Unconditionally publish into the shared cache — this both
        // hydrates a cold slot and *upgrades* any default self-signed
        // entry the fallback manager may have already installed.
        if let Err(e) = self.install_certified_key(tenant, stored) {
            tracing::warn!(tenant, error = %e, "failed to install cached cert");
            return false;
        }
        true
    }

    fn install_certified_key(&self, tenant: &str, stored: store::StoredCert) -> Result<()> {
        let signer = rustls::crypto::ring::sign::any_supported_type(&stored.key)
            .map_err(|e| anyhow!("unable to load private key for {}: {}", tenant, e))?;
        let certified = Arc::new(CertifiedKey::new(stored.chain, signer));
        self.inner
            .cache
            .write()
            .map_err(|_| anyhow!("cache poisoned"))?
            .insert(tenant.to_string(), certified);
        Ok(())
    }

    async fn run_renewal_pass(&self) -> Result<()> {
        let tenants: Vec<String> = self
            .inner
            .known_tenants
            .read()
            .await
            .iter()
            .cloned()
            .collect();
        for tenant in tenants {
            if let Err(e) = self.ensure_cert_inner(&tenant).await {
                tracing::warn!(tenant, error = %e, "renewal failed");
            }
        }
        Ok(())
    }

    async fn issue(&self, tenant: &str) -> Result<()> {
        tracing::info!(tenant, "ACME: starting issuance");
        let apex = tenant.to_string();
        let wildcard = format!("*.{}", apex);
        let identifiers = [
            Identifier::Dns(apex.clone()),
            Identifier::Dns(wildcard.clone()),
        ];
        let mut order = self
            .inner
            .account
            .new_order(&NewOrder::new(&identifiers))
            .await
            .context("ACME new_order")?;

        // Pass 1: for each authorization, publish the DNS-01 TXT record.
        let mut published: Vec<(String, String)> = Vec::new();
        {
            let mut auths = order.authorizations();
            while let Some(result) = auths.next().await {
                let mut auth = result.context("iterating authorizations")?;
                if auth.status == AuthorizationStatus::Valid {
                    continue;
                }
                let challenge = auth
                    .challenge(ChallengeType::Dns01)
                    .ok_or_else(|| anyhow!("no dns-01 challenge offered"))?;
                let dns_value = challenge.key_authorization().dns_value();
                let ident_name = match challenge.identifier().identifier {
                    Identifier::Dns(d) => d.clone(),
                    _ => anyhow::bail!("non-DNS identifier in challenge"),
                };
                // RFC 8555: challenge FQDN strips any leading `*.`.
                let base = ident_name.trim_start_matches("*.");
                let fqdn = format!("_acme-challenge.{}.", base);
                self.inner.httpreq.present(&fqdn, &dns_value).await?;
                published.push((fqdn, dns_value));
            }
        }

        // Wait for DNS propagation before asking the CA to validate.
        tokio::time::sleep(Duration::from_secs(self.inner.config.propagation_wait_secs)).await;

        // Pass 2: signal challenge ready.
        {
            let mut auths = order.authorizations();
            while let Some(result) = auths.next().await {
                let mut auth = result.context("iterating authorizations for set_ready")?;
                if auth.status == AuthorizationStatus::Valid {
                    continue;
                }
                let mut challenge = auth
                    .challenge(ChallengeType::Dns01)
                    .ok_or_else(|| anyhow!("no dns-01 challenge offered"))?;
                challenge.set_ready().await.context("set_ready")?;
            }
        }

        // Poll order until Ready.
        let retry = RetryPolicy::default();
        let status = order.poll_ready(&retry).await.context("poll_ready")?;
        match status {
            OrderStatus::Ready | OrderStatus::Valid => {}
            other => {
                self.cleanup_best_effort(&published).await;
                anyhow::bail!("ACME order reached unexpected state: {:?}", other);
            }
        }

        // Build CSR. rcgen 0.14 uses `serialize_request(&keypair)`.
        let mut params = rcgen::CertificateParams::new(vec![apex.clone(), wildcard.clone()])
            .context("CertificateParams::new")?;
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, apex.clone());
        let keypair = rcgen::KeyPair::generate().context("KeyPair::generate")?;
        let csr = params
            .serialize_request(&keypair)
            .context("serialize_request")?;
        let csr_der = csr.der().to_vec();

        order
            .finalize_csr(&csr_der)
            .await
            .context("ACME finalize_csr")?;

        let chain_pem = order
            .poll_certificate(&retry)
            .await
            .context("poll_certificate")?;

        self.cleanup_best_effort(&published).await;

        // Parse chain PEM → DER chain.
        let mut chain = Vec::new();
        for block in pem::parse_many(chain_pem.as_bytes())? {
            if block.tag() == "CERTIFICATE" {
                chain.push(rustls::pki_types::CertificateDer::from(
                    block.contents().to_vec(),
                ));
            }
        }
        if chain.is_empty() {
            anyhow::bail!("ACME returned empty chain");
        }

        let key_pkcs8_der = keypair.serialize_der();

        store::save_cert(&self.inner.layout, tenant, &chain, &key_pkcs8_der)?;

        let signer_key =
            PrivateKeyDer::Pkcs8(rustls::pki_types::PrivatePkcs8KeyDer::from(key_pkcs8_der));
        let signer = rustls::crypto::ring::sign::any_supported_type(&signer_key)
            .map_err(|e| anyhow!("loading signing key: {}", e))?;
        let certified = Arc::new(CertifiedKey::new(chain, signer));
        self.inner
            .cache
            .write()
            .map_err(|_| anyhow!("cache poisoned"))?
            .insert(tenant.to_string(), certified);

        tracing::info!(tenant, "ACME: cert issued and cached");
        Ok(())
    }

    async fn cleanup_best_effort(&self, published: &[(String, String)]) {
        for (fqdn, value) in published {
            if let Err(e) = self.inner.httpreq.cleanup(fqdn, value).await {
                tracing::warn!(fqdn = %fqdn, error = %e, "httpreq cleanup failed");
            }
        }
    }
}

async fn load_or_create_account(layout: &AcmeStoreLayout, cfg: &AcmeConfig) -> Result<Account> {
    let path = layout.account_file();
    if let Some(raw) = store::load_account_credentials(&path)? {
        let creds: AccountCredentials =
            serde_json::from_str(&raw).context("parsing stored ACME account credentials")?;
        let builder = Account::builder().context("building ACME account client")?;
        let account = builder
            .from_credentials(creds)
            .await
            .context("loading ACME account from credentials")?;
        tracing::info!(path = %path.display(), "reused ACME account");
        return Ok(account);
    }
    let contact = format!("mailto:{}", cfg.contact_email);
    let builder = Account::builder().context("building ACME account client")?;
    let (account, credentials) = builder
        .create(
            &NewAccount {
                contact: &[contact.as_str()],
                terms_of_service_agreed: true,
                only_return_existing: false,
            },
            cfg.directory.clone(),
            None,
        )
        .await
        .context("ACME account create")?;
    let json = serde_json::to_string(&credentials).context("serialize account credentials")?;
    store::save_account_credentials(&path, &json)?;
    tracing::info!(path = %path.display(), "created ACME account");
    Ok(account)
}

fn hydrate_cache_from_disk(inner: &AcmeInner) -> Result<()> {
    let certs_dir = inner.layout.certs_dir();
    if !certs_dir.is_dir() {
        return Ok(());
    }
    let mut cache_write = inner.cache.write().map_err(|_| anyhow!("cache poisoned"))?;
    for entry in std::fs::read_dir(&certs_dir)? {
        let entry = entry?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(tenant) = name.strip_suffix(".pem") else {
            continue;
        };
        match store::load_cert(&inner.layout, tenant) {
            Ok(Some(stored)) => match rustls::crypto::ring::sign::any_supported_type(&stored.key) {
                Ok(signer) => {
                    let certified = Arc::new(CertifiedKey::new(stored.chain, signer));
                    cache_write.insert(tenant.to_string(), certified);
                    tracing::info!(tenant, "cached cert from disk");
                }
                Err(e) => tracing::warn!(tenant, error = %e, "bad signing key"),
            },
            Ok(None) => {}
            Err(e) => tracing::warn!(tenant, error = %e, "load_cert failed"),
        }
    }
    // Record hydrated tenants as known so the renewal loop covers them
    // even before any tunnel session arrives in this process lifetime.
    let mut known = HashMap::new();
    for t in cache_write.keys() {
        known.insert(t.clone(), ());
    }
    drop(cache_write);
    // Fill known_tenants without blocking on async lock from sync context.
    if let Ok(mut w) = inner.known_tenants.try_write() {
        for (k, _) in known {
            w.insert(k);
        }
    }
    Ok(())
}

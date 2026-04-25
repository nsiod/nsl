//! rustls `ResolvesServerCert` implementation backed by the ACME cache.
//!
//! On every ClientHello, `resolve` is called with the SNI host. We derive
//! the tenant apex (the single label above `base_domain`) and look it up
//! in the `Arc<RwLock<HashMap<tenant, CertifiedKey>>>` the
//! [`AcmeManager`](super::AcmeManager) publishes into.
//!
//! A miss returns `None`, aborting the TLS handshake. We intentionally
//! don't serve a placeholder cert because that just triggers browser
//! security warnings for no benefit — the tenant isn't online anyway
//! and the app will 502 even if TLS succeeded.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;

pub type CertCache = Arc<RwLock<HashMap<String, Arc<CertifiedKey>>>>;

pub fn new_cache() -> CertCache {
    Arc::new(RwLock::new(HashMap::new()))
}

/// Reduce an SNI like `myapp.alice.nsl.example.com` to the tenant apex
/// `alice.nsl.example.com` when `base_domain = "nsl.example.com"`. If the
/// SNI equals the apex itself, returns it unchanged. Returns `None` when
/// the SNI doesn't sit under `base_domain` at all.
pub fn tenant_from_sni(sni: &str, base_domain: &str) -> Option<String> {
    let sni = sni.trim_end_matches('.').to_ascii_lowercase();
    let base = base_domain.trim_end_matches('.').to_ascii_lowercase();
    if sni == base {
        // The bare apex of the base domain is never a tenant — reject.
        return None;
    }
    let suffix = format!(".{}", base);
    let head = sni.strip_suffix(&suffix)?;
    // `head` is everything before `.base_domain`. We want the last label
    // of head (tenant), glued back onto base_domain.
    let tenant_label = head.rsplit('.').next()?;
    if tenant_label.is_empty() {
        return None;
    }
    Some(format!("{}.{}", tenant_label, base))
}

pub struct AcmeResolver {
    cache: CertCache,
    base_domain: String,
}

impl AcmeResolver {
    pub fn new(cache: CertCache, base_domain: String) -> Self {
        Self { cache, base_domain }
    }
}

impl std::fmt::Debug for AcmeResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcmeResolver")
            .field("base_domain", &self.base_domain)
            .finish()
    }
}

impl ResolvesServerCert for AcmeResolver {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let sni = hello.server_name()?;
        let tenant = tenant_from_sni(sni, &self.base_domain)?;
        let guard = self.cache.read().ok()?;
        guard.get(&tenant).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tenant_extraction() {
        assert_eq!(
            tenant_from_sni("myapp.alice.nsl.example.com", "nsl.example.com"),
            Some("alice.nsl.example.com".into())
        );
        assert_eq!(
            tenant_from_sni("alice.nsl.example.com", "nsl.example.com"),
            Some("alice.nsl.example.com".into())
        );
        assert_eq!(
            tenant_from_sni("a.b.alice.nsl.example.com", "nsl.example.com"),
            Some("alice.nsl.example.com".into())
        );
    }

    #[test]
    fn rejects_base_domain_alone() {
        assert_eq!(tenant_from_sni("nsl.example.com", "nsl.example.com"), None);
    }

    #[test]
    fn rejects_unrelated_domains() {
        assert_eq!(
            tenant_from_sni("foo.other.example", "nsl.example.com"),
            None
        );
    }

    #[test]
    fn case_and_trailing_dot_normalization() {
        assert_eq!(
            tenant_from_sni("Myapp.Alice.NSL.Example.com.", "nsl.example.com"),
            Some("alice.nsl.example.com".into())
        );
    }
}

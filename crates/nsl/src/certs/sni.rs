use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rustls::server::ResolvesServerCert;
use rustls::sign::CertifiedKey;

use super::{HOST_CERTS_DIR, generate_host_cert, is_cert_valid, load_certified_key};

// ---------------------------------------------------------------------------
// Hostname validation / sanitization
// ---------------------------------------------------------------------------

/// Sanitize a hostname for safe use as a filesystem path component.
/// Only allows alphanumeric characters, hyphens, and dots (replaced with
/// underscores). All other characters (including `/`, `\`, null bytes) are
/// replaced with underscores to prevent path traversal.
pub fn sanitize_hostname_for_filename(hostname: &str) -> String {
    hostname
        .chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' => c,
            '.' => '_',
            _ => '_',
        })
        .collect()
}

/// Validate that a hostname is safe for on-demand certificate generation.
/// Rejects empty hostnames, those exceeding 253 characters (DNS max), and
/// those containing path traversal sequences.
pub(super) fn validate_sni_hostname(hostname: &str) -> bool {
    if hostname.is_empty() || hostname.len() > 253 {
        return false;
    }
    if hostname.contains('/') || hostname.contains('\\') || hostname.contains('\0') {
        return false;
    }
    if hostname.contains("..") {
        return false;
    }
    hostname
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
}

// ---------------------------------------------------------------------------
// SNI resolver
// ---------------------------------------------------------------------------

/// Maximum number of cached per-hostname certs to prevent memory exhaustion
/// from a large number of distinct SNI hostnames.
const MAX_SNI_CACHE_SIZE: usize = 1000;

/// A rustls `ResolvesServerCert` implementation that serves per-hostname
/// certificates. Unknown hostnames trigger on-demand certificate generation
/// signed by the local CA.
pub struct SniCertResolver {
    /// Default server cert (for "localhost" and unknown SNI).
    default_key: Arc<CertifiedKey>,
    /// In-memory cache of per-hostname certified keys. Also guards
    /// concurrent generation: presence in the map means cert is ready.
    cache: Mutex<HashMap<String, Arc<CertifiedKey>>>,
    /// State directory for cert storage.
    state_dir: PathBuf,
}

impl std::fmt::Debug for SniCertResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SniCertResolver")
            .field("state_dir", &self.state_dir)
            .finish_non_exhaustive()
    }
}

impl SniCertResolver {
    /// Create a new SNI resolver from a default server cert/key and state dir.
    pub fn new(cert_path: &Path, key_path: &Path, state_dir: PathBuf) -> anyhow::Result<Self> {
        let default_key = Arc::new(load_certified_key(cert_path, key_path)?);
        Ok(Self {
            default_key,
            cache: Mutex::new(HashMap::new()),
            state_dir,
        })
    }

    /// Resolve a certified key for the given hostname. For "localhost" or
    /// empty SNI, returns the default key. For other hostnames, looks up
    /// the disk cache, generates on demand if needed.
    fn resolve_for_hostname(&self, hostname: &str) -> Arc<CertifiedKey> {
        if hostname.is_empty() || hostname == "localhost" {
            return Arc::clone(&self.default_key);
        }

        if !validate_sni_hostname(hostname) {
            tracing::warn!("rejected invalid SNI hostname: {:?}", hostname);
            return Arc::clone(&self.default_key);
        }

        // Hold the lock for the entire resolve to prevent concurrent
        // generation for the same hostname. This resolver is a synchronous
        // rustls callback, so use a synchronous mutex here.
        let mut cache = match self.cache.lock() {
            Ok(cache) => cache,
            Err(_) => {
                tracing::warn!("SNI certificate cache is poisoned");
                return Arc::clone(&self.default_key);
            }
        };

        if let Some(key) = cache.get(hostname) {
            return Arc::clone(key);
        }

        // Try disk cache.
        let safe_name = sanitize_hostname_for_filename(hostname);
        let host_dir = self.state_dir.join(HOST_CERTS_DIR);
        let cert_path = host_dir.join(format!("{}.pem", safe_name));
        let key_path = host_dir.join(format!("{}-key.pem", safe_name));

        if cert_path.exists()
            && key_path.exists()
            && is_cert_valid(&cert_path)
            && let Ok(ck) = load_certified_key(&cert_path, &key_path)
        {
            let ck = Arc::new(ck);
            if cache.len() < MAX_SNI_CACHE_SIZE {
                cache.insert(hostname.to_string(), Arc::clone(&ck));
            }
            return ck;
        }

        // Generate on demand (still under lock to prevent races).
        match generate_host_cert(&self.state_dir, hostname) {
            Ok((cert_p, key_p)) => {
                if let Ok(ck) = load_certified_key(&cert_p, &key_p) {
                    let ck = Arc::new(ck);
                    if cache.len() < MAX_SNI_CACHE_SIZE {
                        cache.insert(hostname.to_string(), Arc::clone(&ck));
                    }
                    return ck;
                }
            }
            Err(e) => {
                tracing::warn!("failed to generate cert for {}: {}", hostname, e);
            }
        }

        Arc::clone(&self.default_key)
    }
}

impl ResolvesServerCert for SniCertResolver {
    fn resolve(&self, client_hello: rustls::server::ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let hostname = client_hello.server_name().unwrap_or("");
        Some(self.resolve_for_hostname(hostname))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::super::{SERVER_CERT_FILE, SERVER_KEY_FILE, ensure_certs};
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_sanitize_hostname_for_filename() {
        assert_eq!(
            sanitize_hostname_for_filename("myapp.localhost"),
            "myapp_localhost"
        );
        assert_eq!(sanitize_hostname_for_filename("a.b.c"), "a_b_c");
        assert_eq!(sanitize_hostname_for_filename("localhost"), "localhost");
        assert_eq!(
            sanitize_hostname_for_filename("../../etc/passwd"),
            "______etc_passwd"
        );
        assert_eq!(sanitize_hostname_for_filename("foo/bar"), "foo_bar");
        assert_eq!(sanitize_hostname_for_filename("a\0b"), "a_b");
    }

    #[test]
    fn test_validate_sni_hostname() {
        assert!(validate_sni_hostname("myapp.localhost"));
        assert!(validate_sni_hostname("a-b.localhost"));
        assert!(validate_sni_hostname("localhost"));
        assert!(!validate_sni_hostname("../../etc/passwd"));
        assert!(!validate_sni_hostname("foo/bar"));
        assert!(!validate_sni_hostname("foo\\bar"));
        assert!(!validate_sni_hostname("foo..bar"));
        assert!(!validate_sni_hostname(""));
        assert!(!validate_sni_hostname(&"a".repeat(254)));
        assert!(!validate_sni_hostname("foo\0bar"));
    }

    #[test]
    fn test_sni_resolver_default_for_localhost() {
        let tmp = TempDir::new().unwrap();
        ensure_certs(tmp.path()).unwrap();

        let resolver = SniCertResolver::new(
            &tmp.path().join(SERVER_CERT_FILE),
            &tmp.path().join(SERVER_KEY_FILE),
            tmp.path().to_path_buf(),
        )
        .unwrap();

        let key = resolver.resolve_for_hostname("localhost");
        assert!(!key.cert.is_empty(), "should return a cert for localhost");
    }

    #[test]
    fn test_sni_resolver_generates_host_cert() {
        let tmp = TempDir::new().unwrap();
        ensure_certs(tmp.path()).unwrap();

        let resolver = SniCertResolver::new(
            &tmp.path().join(SERVER_CERT_FILE),
            &tmp.path().join(SERVER_KEY_FILE),
            tmp.path().to_path_buf(),
        )
        .unwrap();

        let key = resolver.resolve_for_hostname("myapp.localhost");
        assert!(
            !key.cert.is_empty(),
            "should return a cert for myapp.localhost"
        );

        let host_cert = tmp.path().join(HOST_CERTS_DIR).join("myapp_localhost.pem");
        assert!(host_cert.exists(), "host cert should be cached on disk");
    }

    #[test]
    fn test_sni_resolver_empty_sni_uses_default() {
        let tmp = TempDir::new().unwrap();
        ensure_certs(tmp.path()).unwrap();

        let resolver = SniCertResolver::new(
            &tmp.path().join(SERVER_CERT_FILE),
            &tmp.path().join(SERVER_KEY_FILE),
            tmp.path().to_path_buf(),
        )
        .unwrap();

        let key = resolver.resolve_for_hostname("");
        assert!(!key.cert.is_empty());
    }
}

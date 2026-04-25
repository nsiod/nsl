//! Live session registry with two lookup axes:
//!   - by tenant domain (used by the public HTTP listener)
//!   - by server-assigned session id (used for diagnostics / future
//!     admin endpoints)
//!
//! Both indices share the same `Arc<Session>` values so inserts and
//! removals stay consistent. Session identity is `Arc::ptr_eq`; a stale
//! remove that races a preempting insert is a no-op.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::RwLock;

/// A live tenant session authenticated on the server.
pub struct Session {
    pub domain: String,
    pub session_id: String,
    pub connection: quinn::Connection,
}

#[derive(Default)]
struct Inner {
    by_domain: HashMap<String, Arc<Session>>,
    by_session_id: HashMap<String, Arc<Session>>,
}

/// Thread-safe dual-index session registry.
#[derive(Clone, Default)]
pub struct SessionRegistry {
    inner: Arc<RwLock<Inner>>,
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a session, preempting any existing session for the same
    /// domain. Returns the old session (if any) so the caller can close
    /// its connection outside the registry lock.
    pub async fn insert(&self, session: Arc<Session>) -> Option<Arc<Session>> {
        let key_domain = session.domain.to_lowercase();
        let key_id = session.session_id.clone();
        let mut w = self.inner.write().await;
        let old = w.by_domain.insert(key_domain, Arc::clone(&session));
        if let Some(ref old) = old {
            w.by_session_id.remove(&old.session_id);
        }
        w.by_session_id.insert(key_id, session);
        old
    }

    /// Remove the session if-and-only-if it's still the one indexed
    /// under its domain. Prevents a late remove from evicting a newer
    /// session installed by a preempt.
    pub async fn remove_if_current(&self, session: &Arc<Session>) {
        let key_domain = session.domain.to_lowercase();
        let mut w = self.inner.write().await;
        let same = w
            .by_domain
            .get(&key_domain)
            .map(|cur| Arc::ptr_eq(cur, session))
            .unwrap_or(false);
        if same {
            w.by_domain.remove(&key_domain);
            w.by_session_id.remove(&session.session_id);
        }
    }

    /// Find a session whose tenant domain either equals `host` exactly
    /// or is a suffix match on `".<tenant_domain>"`.
    pub async fn find_by_host(&self, host: &str) -> Option<Arc<Session>> {
        let host = host.to_ascii_lowercase();
        let r = self.inner.read().await;
        for (tenant, sess) in r.by_domain.iter() {
            if host == *tenant {
                return Some(Arc::clone(sess));
            }
            let suffix = format!(".{}", tenant);
            if host.ends_with(&suffix) {
                return Some(Arc::clone(sess));
            }
        }
        None
    }

    pub async fn find_by_session_id(&self, id: &str) -> Option<Arc<Session>> {
        let r = self.inner.read().await;
        r.by_session_id.get(id).cloned()
    }

    pub async fn len(&self) -> usize {
        self.inner.read().await.by_domain.len()
    }

    pub async fn is_empty(&self) -> bool {
        self.inner.read().await.by_domain.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // We can't construct a real `quinn::Connection` in unit tests (it
    // requires a live QUIC endpoint). The e2e test covers end-to-end
    // insertion + lookup paths; here we only check the pure-data logic
    // that doesn't touch the connection field.

    #[test]
    fn host_matches_suffix_logic() {
        // Exercise the same lowercase/suffix rule SessionRegistry uses
        // without instantiating a registry (which would require a
        // connection).
        fn match_key(host: &str, keys: &[&str]) -> Option<String> {
            let host = host.to_ascii_lowercase();
            for k in keys {
                if host == *k {
                    return Some(k.to_string());
                }
                let s = format!(".{}", k);
                if host.ends_with(&s) {
                    return Some(k.to_string());
                }
            }
            None
        }
        assert_eq!(
            match_key("myapp.alice.example.com", &["alice.example.com"]),
            Some("alice.example.com".to_string())
        );
        assert_eq!(
            match_key("ALICE.example.com", &["alice.example.com"]),
            Some("alice.example.com".to_string())
        );
        assert_eq!(match_key("other.example.com", &["alice.example.com"]), None);
    }

    fn _assert_send_sync() {
        fn sync<T: Send + Sync>() {}
        sync::<SessionRegistry>();
    }
}

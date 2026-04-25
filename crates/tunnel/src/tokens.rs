//! Token store (server side).
//!
//! Loaded from a TOML file. Each entry is a nested table keyed by the
//! client handle — that handle is the `id` the client sends in the
//! `Hello` frame. The table value contains the tenant domain the server
//! is willing to route for that client and a shared HMAC key.
//!
//! Example `tokens.toml`:
//!
//! ```toml
//! [tokens.alice]
//! domain = "alice.nsl.example.com"         # server-assigned public domain
//! key    = "nslk_..."
//!
//! [tokens.bob]
//! domain = "bob.nsl.example.com"
//! key    = "nslk_..."
//! ```
//!
//! Keys are stored in plaintext for MVP simplicity. A future version may
//! switch to hashed storage.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use tokio::sync::RwLock;

#[derive(Debug, Deserialize)]
struct RawTokensFile {
    #[serde(default)]
    tokens: HashMap<String, RawToken>,
}

#[derive(Debug, Deserialize)]
struct RawToken {
    domain: String,
    key: String,
}

/// One resolved token — what the server looks up on each handshake.
#[derive(Debug, Clone)]
pub struct TokenEntry {
    pub domain: String,
    pub key: String,
}

/// Immutable in-memory token index keyed by client identifier.
#[derive(Debug, Clone, Default)]
pub struct TokenStore {
    by_id: HashMap<String, TokenEntry>,
}

impl TokenStore {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("failed to read tokens file {}: {}", path.display(), e))?;
        Self::from_toml(&content)
    }

    pub fn from_toml(content: &str) -> anyhow::Result<Self> {
        let raw: RawTokensFile = toml::from_str(content)
            .map_err(|e| anyhow::anyhow!("failed to parse tokens file: {}", e))?;
        let mut by_id = HashMap::with_capacity(raw.tokens.len());
        for (id, t) in raw.tokens {
            let id = id.trim().to_lowercase();
            let domain = t.domain.trim().to_lowercase();
            if id.is_empty() || domain.is_empty() || t.key.is_empty() {
                continue;
            }
            by_id.insert(
                id,
                TokenEntry {
                    domain,
                    key: t.key,
                },
            );
        }
        Ok(Self { by_id })
    }

    /// Resolve a client id to its assigned domain + key.
    pub fn lookup(&self, client_id: &str) -> Option<&TokenEntry> {
        self.by_id.get(&client_id.to_lowercase())
    }

    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }
}

/// Shared, hot-reloadable view of the token store.
///
/// The server reads from this on every handshake; the filesystem watcher
/// swaps the inner snapshot atomically via `replace`. `Arc` + `RwLock`
/// gives cheap concurrent readers and brief exclusive writes on reload.
#[derive(Clone)]
pub struct SharedTokenStore {
    inner: Arc<RwLock<TokenStore>>,
}

impl SharedTokenStore {
    pub fn new(store: TokenStore) -> Self {
        Self {
            inner: Arc::new(RwLock::new(store)),
        }
    }

    pub async fn lookup(&self, client_id: &str) -> Option<TokenEntry> {
        let r = self.inner.read().await;
        r.lookup(client_id).cloned()
    }

    pub async fn len(&self) -> usize {
        self.inner.read().await.len()
    }

    pub async fn is_empty(&self) -> bool {
        self.inner.read().await.is_empty()
    }

    pub async fn replace(&self, next: TokenStore) {
        let mut w = self.inner.write().await;
        *w = next;
    }
}

/// Spawn a best-effort filesystem watcher that reloads the token store
/// whenever `path` changes. Returns the shared handle and the watcher
/// task; dropping the task stops the watcher.
///
/// Reload errors are logged and the previous snapshot is kept.
pub fn spawn_hot_reload(
    path: PathBuf,
    initial: TokenStore,
) -> (SharedTokenStore, tokio::task::JoinHandle<()>) {
    use notify::{Event, EventKind, RecursiveMode, Watcher};

    let shared = SharedTokenStore::new(initial);
    let shared_for_task = shared.clone();
    let path_clone = path.clone();

    let handle = tokio::spawn(async move {
        // Use a bounded channel: notify's watcher callback runs off-thread
        // and we don't care about back-pressure — dropping duplicate
        // events during a burst is fine because we always reload from
        // disk.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(16);
        let tx_cb = tx.clone();

        let mut watcher = match notify::recommended_watcher(move |res: notify::Result<Event>| {
            if let Ok(event) = res
                && matches!(
                    event.kind,
                    EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_)
                )
            {
                let _ = tx_cb.try_send(());
            }
        }) {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!(error = %e, "token watcher init failed; hot-reload disabled");
                return;
            }
        };

        // Watch the directory containing the tokens file, since editors
        // commonly replace the file atomically (rename) which breaks
        // watches bound directly to the inode.
        let watch_target = path_clone
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        if let Err(e) = watcher.watch(&watch_target, RecursiveMode::NonRecursive) {
            tracing::warn!(path = %watch_target.display(), error = %e, "watch failed; hot-reload disabled");
            return;
        }
        tracing::info!(path = %path_clone.display(), "tokens hot-reload watcher started");

        while rx.recv().await.is_some() {
            // Debounce: drain any events that arrived while we were
            // about to reload, then wait briefly for the writer to
            // finish before reading.
            tokio::time::sleep(Duration::from_millis(50)).await;
            while rx.try_recv().is_ok() {}

            match TokenStore::load(&path_clone) {
                Ok(next) => {
                    let count = next.len();
                    shared_for_task.replace(next).await;
                    tracing::info!(tokens = count, "tokens reloaded");
                }
                Err(e) => {
                    tracing::warn!(error = %e, "tokens reload failed; keeping previous snapshot");
                }
            }
        }
    });

    (shared, handle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_multi_token_file() {
        let toml = r#"
            [tokens.alice]
            domain = "alice.nsl.example.com"
            key    = "k1"

            [tokens.bob-laptop]
            domain = "bob.nsl.example.com"
            key    = "k2"
        "#;
        let store = TokenStore::from_toml(toml).unwrap();
        assert_eq!(store.len(), 2);
        let a = store.lookup("alice").unwrap();
        assert_eq!(a.domain, "alice.nsl.example.com");
        assert_eq!(a.key, "k1");
        let b = store.lookup("bob-laptop").unwrap();
        assert_eq!(b.domain, "bob.nsl.example.com");
    }

    #[test]
    fn lookup_is_case_insensitive_on_id() {
        let toml = r#"
            [tokens.Alice]
            domain = "alice.nsl.example.com"
            key    = "k1"
        "#;
        let store = TokenStore::from_toml(toml).unwrap();
        assert!(store.lookup("alice").is_some());
        assert!(store.lookup("ALICE").is_some());
    }

    #[test]
    fn unknown_id_rejected() {
        let store = TokenStore::from_toml("").unwrap();
        assert!(store.lookup("anything").is_none());
    }

    #[test]
    fn empty_entries_skipped() {
        let toml = r#"
            [tokens.bad-domain]
            domain = ""
            key    = "k1"

            [tokens.bad-key]
            domain = "valid"
            key    = ""
        "#;
        let store = TokenStore::from_toml(toml).unwrap();
        assert!(store.is_empty());
    }
}

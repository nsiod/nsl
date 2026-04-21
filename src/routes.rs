use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::error::NSLError;
use crate::platform::is_process_alive;
use crate::utils::fix_ownership;

/// How long (ms) before a lock directory is considered stale.
const STALE_LOCK_THRESHOLD: Duration = Duration::from_secs(10);

/// Maximum number of retries when acquiring the file lock.
const LOCK_MAX_RETRIES: u32 = 20;

/// Delay between lock acquisition retries.
const LOCK_RETRY_DELAY: Duration = Duration::from_millis(50);

/// A route mapping stored on disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteMapping {
    pub hostname: String,
    pub port: u16,
    pub pid: u32,
    /// When true, rewrite the Host header to `127.0.0.1:<port>` before
    /// forwarding. Useful for backends that validate the Host/Origin header.
    #[serde(default)]
    pub change_origin: bool,
    /// Path prefix for matching (default: "/").
    #[serde(default = "default_path_prefix")]
    pub path_prefix: String,
    /// Strip the matched prefix before forwarding.
    #[serde(default)]
    pub strip_prefix: bool,
}

fn default_path_prefix() -> String {
    "/".to_string()
}

/// Normalize a path prefix: ensure leading slash, strip trailing slash
/// (unless it is the root "/").
pub fn normalize_path_prefix(prefix: &str) -> String {
    let mut p = prefix.to_string();
    if !p.starts_with('/') {
        p.insert(0, '/');
    }
    // Remove trailing slash unless it's the root
    if p.len() > 1 && p.ends_with('/') {
        p.pop();
    }
    p
}

/// Manages route mappings stored as a JSON file on disk.
pub struct RouteStore {
    dir: PathBuf,
    routes_path: PathBuf,
    lock_path: PathBuf,
}

impl RouteStore {
    pub fn new(dir: PathBuf) -> Self {
        let routes_path = dir.join("routes.json");
        let lock_path = dir.join("routes.lock");
        Self {
            dir,
            routes_path,
            lock_path,
        }
    }

    pub fn ensure_dir(&self) -> Result<(), NSLError> {
        if !self.dir.exists() {
            fs::create_dir_all(&self.dir)?;
        }
        Ok(())
    }

    #[allow(dead_code)]
    pub fn routes_path(&self) -> &Path {
        &self.routes_path
    }

    /// Load routes from disk, filtering out stale entries.
    pub fn load_routes(&self) -> Result<Vec<RouteMapping>, NSLError> {
        if !self.routes_path.exists() {
            return Ok(Vec::new());
        }

        let raw = fs::read_to_string(&self.routes_path)?;
        let routes: Vec<RouteMapping> = serde_json::from_str(&raw)?;

        let alive: Vec<RouteMapping> = routes
            .into_iter()
            .filter(|r| r.pid == 0 || is_process_alive(r.pid))
            .collect();

        Ok(alive)
    }

    /// Add a route, checking for conflicts on hostname + path_prefix.
    pub fn add_route(
        &self,
        hostname: &str,
        port: u16,
        pid: u32,
        force: bool,
        change_origin: bool,
        path_prefix: &str,
        strip_prefix: bool,
    ) -> Result<(), NSLError> {
        self.ensure_dir()?;
        let _lock = self.acquire_lock()?;

        let norm_prefix = normalize_path_prefix(path_prefix);
        let mut routes = self.load_routes()?;

        if let Some(existing) = routes.iter().find(|r| {
            r.hostname == hostname && normalize_path_prefix(&r.path_prefix) == norm_prefix
        }) && existing.pid != pid
            && (existing.pid == 0 || is_process_alive(existing.pid))
        {
            if !force {
                return Err(NSLError::RouteConflict {
                    hostname: hostname.to_string(),
                    path_prefix: norm_prefix,
                    pid: existing.pid,
                });
            }
            // force=true: kill the orphaned app process before replacing the route
            if existing.pid != 0 && is_process_alive(existing.pid) {
                crate::platform::kill_app_process(existing.pid);
            }
        }

        routes.retain(|r| {
            !(r.hostname == hostname && normalize_path_prefix(&r.path_prefix) == norm_prefix)
        });
        routes.push(RouteMapping {
            hostname: hostname.to_string(),
            port,
            pid,
            change_origin,
            path_prefix: norm_prefix,
            strip_prefix,
        });

        self.save_routes(&routes)?;
        Ok(())
    }

    /// Remove a route by hostname, optionally filtered by path_prefix.
    ///
    /// If `path_prefix` is `None`, all routes for the hostname are removed.
    /// If `path_prefix` is `Some(prefix)`, only the route with the matching
    /// hostname + path_prefix is removed.
    pub fn remove_route(&self, hostname: &str, path_prefix: Option<&str>) -> Result<(), NSLError> {
        self.ensure_dir()?;
        let _lock = self.acquire_lock()?;

        let mut routes = self.load_routes()?;
        match path_prefix {
            Some(prefix) => {
                let norm = normalize_path_prefix(prefix);
                routes.retain(|r| {
                    !(r.hostname == hostname && normalize_path_prefix(&r.path_prefix) == norm)
                });
            }
            None => {
                routes.retain(|r| r.hostname != hostname);
            }
        }
        self.save_routes(&routes)?;
        Ok(())
    }

    fn save_routes(&self, routes: &[RouteMapping]) -> Result<(), NSLError> {
        let json = serde_json::to_string_pretty(routes)?;
        fs::write(&self.routes_path, json)?;
        fix_ownership(&self.routes_path);
        Ok(())
    }

    fn acquire_lock(&self) -> Result<LockGuard, NSLError> {
        for _ in 0..LOCK_MAX_RETRIES {
            match fs::create_dir(&self.lock_path) {
                Ok(()) => {
                    return Ok(LockGuard {
                        path: self.lock_path.clone(),
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    // Check for stale lock
                    if let Ok(meta) = fs::metadata(&self.lock_path)
                        && let Ok(modified) = meta.modified()
                        && SystemTime::now()
                            .duration_since(modified)
                            .unwrap_or_default()
                            > STALE_LOCK_THRESHOLD
                    {
                        let _ = fs::remove_dir_all(&self.lock_path);
                        continue;
                    }
                    std::thread::sleep(LOCK_RETRY_DELAY);
                }
                Err(e) => return Err(NSLError::Io(e)),
            }
        }
        Err(NSLError::LockFailed)
    }
}

/// RAII guard that releases the file lock on drop.
struct LockGuard {
    path: PathBuf,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_add_and_load_route() {
        let tmp = TempDir::new().unwrap();
        let store = RouteStore::new(tmp.path().to_path_buf());

        store
            .add_route("myapp.localhost", 4000, 0, false, false, "/", false)
            .unwrap();

        let routes = store.load_routes().unwrap();
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].hostname, "myapp.localhost");
        assert_eq!(routes[0].port, 4000);
        assert_eq!(routes[0].path_prefix, "/");
        assert!(!routes[0].strip_prefix);
    }

    #[test]
    fn test_remove_route() {
        let tmp = TempDir::new().unwrap();
        let store = RouteStore::new(tmp.path().to_path_buf());

        store
            .add_route("myapp.localhost", 4000, 0, false, false, "/", false)
            .unwrap();
        store.remove_route("myapp.localhost", None).unwrap();

        let routes = store.load_routes().unwrap();
        assert!(routes.is_empty());
    }

    #[test]
    fn test_route_conflict() {
        let tmp = TempDir::new().unwrap();
        let store = RouteStore::new(tmp.path().to_path_buf());

        // pid 0 means "static route", always considered alive
        store
            .add_route("myapp.localhost", 4000, 0, false, false, "/", false)
            .unwrap();

        // Same hostname + path without force should conflict
        let result = store.add_route("myapp.localhost", 4001, 99999, false, false, "/", false);
        assert!(result.is_err());

        // With force should succeed
        store
            .add_route("myapp.localhost", 4001, 0, true, false, "/", false)
            .unwrap();
        let routes = store.load_routes().unwrap();
        assert_eq!(routes[0].port, 4001);
    }

    #[test]
    fn test_same_hostname_different_paths() {
        let tmp = TempDir::new().unwrap();
        let store = RouteStore::new(tmp.path().to_path_buf());

        store
            .add_route("myapp.localhost", 4000, 0, false, false, "/", false)
            .unwrap();
        store
            .add_route("myapp.localhost", 4001, 0, false, false, "/api", false)
            .unwrap();
        store
            .add_route("myapp.localhost", 4002, 0, false, false, "/api/v2", true)
            .unwrap();

        let routes = store.load_routes().unwrap();
        assert_eq!(routes.len(), 3);
    }

    #[test]
    fn test_conflict_checks_path_prefix() {
        let tmp = TempDir::new().unwrap();
        let store = RouteStore::new(tmp.path().to_path_buf());

        store
            .add_route("myapp.localhost", 4000, 0, false, false, "/api", false)
            .unwrap();

        // Same hostname + different path should NOT conflict
        let result = store.add_route("myapp.localhost", 4001, 99999, false, false, "/web", false);
        assert!(result.is_ok());

        // Same hostname + same path should conflict
        let result = store.add_route("myapp.localhost", 4002, 99999, false, false, "/api", false);
        assert!(result.is_err());
    }

    #[test]
    fn test_remove_route_with_path_prefix() {
        let tmp = TempDir::new().unwrap();
        let store = RouteStore::new(tmp.path().to_path_buf());

        store
            .add_route("myapp.localhost", 4000, 0, false, false, "/", false)
            .unwrap();
        store
            .add_route("myapp.localhost", 4001, 0, false, false, "/api", false)
            .unwrap();

        // Remove only the /api route
        store.remove_route("myapp.localhost", Some("/api")).unwrap();
        let routes = store.load_routes().unwrap();
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].path_prefix, "/");
    }

    #[test]
    fn test_remove_route_all_for_hostname() {
        let tmp = TempDir::new().unwrap();
        let store = RouteStore::new(tmp.path().to_path_buf());

        store
            .add_route("myapp.localhost", 4000, 0, false, false, "/", false)
            .unwrap();
        store
            .add_route("myapp.localhost", 4001, 0, false, false, "/api", false)
            .unwrap();

        // Remove all routes for hostname
        store.remove_route("myapp.localhost", None).unwrap();
        let routes = store.load_routes().unwrap();
        assert!(routes.is_empty());
    }

    #[test]
    fn test_normalize_path_prefix() {
        assert_eq!(normalize_path_prefix("/"), "/");
        assert_eq!(normalize_path_prefix("/api"), "/api");
        assert_eq!(normalize_path_prefix("/api/"), "/api");
        assert_eq!(normalize_path_prefix("api"), "/api");
        assert_eq!(normalize_path_prefix("api/"), "/api");
    }

    #[test]
    fn test_trailing_slash_equivalence() {
        let tmp = TempDir::new().unwrap();
        let store = RouteStore::new(tmp.path().to_path_buf());

        store
            .add_route("myapp.localhost", 4000, 0, false, false, "/api", false)
            .unwrap();

        // "/api/" should conflict with "/api" (trailing slash normalized)
        let result = store.add_route("myapp.localhost", 4001, 99999, false, false, "/api/", false);
        assert!(result.is_err());
    }

    #[test]
    fn test_backward_compat_deserialization() {
        let tmp = TempDir::new().unwrap();
        let store = RouteStore::new(tmp.path().to_path_buf());
        store.ensure_dir().unwrap();

        // Write a JSON file without path_prefix or strip_prefix (old format)
        let old_json = r#"[{"hostname":"myapp.localhost","port":4000,"pid":0}]"#;
        std::fs::write(store.routes_path(), old_json).unwrap();

        let routes = store.load_routes().unwrap();
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].path_prefix, "/");
        assert!(!routes[0].strip_prefix);
    }
}

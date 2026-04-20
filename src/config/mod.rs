use std::collections::BTreeMap;
use std::fs;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::utils::{DEFAULT_PORT, PRIVILEGED_PORT_THRESHOLD};

/// Raw TOML config with all fields optional (for layered merging).
#[derive(Debug, Default, Deserialize, Serialize, Clone)]
pub struct RawConfig {
    pub proxy: Option<RawProxy>,
    pub app: Option<RawApp>,
    pub paths: Option<RawPaths>,
}

#[derive(Debug, Default, Deserialize, Serialize, Clone)]
pub struct RawProxy {
    pub listen: Option<String>,
    // Deprecated: use `listen` instead. Kept for older config files.
    pub port: Option<u16>,
    pub https: Option<bool>,
    pub max_hops: Option<u8>,
    /// Custom domain suffixes (e.g. ["localhost", "dev.local", "test"]).
    /// `localhost` is always implicitly included; user entries are additive.
    pub domains: Option<Vec<String>>,
    /// Per-domain display overrides, keyed by domain suffix.
    /// Useful when external domains are fronted by a reverse proxy
    /// (scheme/port differ from the local proxy). Example TOML:
    ///
    /// ```toml
    /// [proxy.display."myapp.com"]
    /// scheme = "https"
    /// port = 8080
    /// ```
    pub display: Option<BTreeMap<String, RawDomainDisplay>>,
    /// Deprecated: use `listen` instead. Kept for older config files.
    pub bind: Option<String>,
}

/// Display override values for a single domain suffix.
/// The suffix itself comes from the surrounding map key.
#[derive(Debug, Default, Deserialize, Serialize, Clone)]
pub struct RawDomainDisplay {
    /// Whether the URL uses HTTPS. Default: true.
    pub https: Option<bool>,
    /// URL port. Omit for the scheme's default (80/443).
    pub port: Option<u16>,
}

#[derive(Debug, Default, Deserialize, Serialize, Clone)]
pub struct RawApp {
    pub port: Option<u16>,
    pub port_range_start: Option<u16>,
    pub port_range_end: Option<u16>,
    pub force: Option<bool>,
    pub ready_timeout: Option<u64>,
}

#[derive(Debug, Default, Deserialize, Serialize, Clone)]
pub struct RawPaths {
    pub state_dir: Option<String>,
}

/// Resolved config with concrete values (defaults applied).
#[derive(Debug, Clone)]
pub struct Config {
    pub proxy_port: u16,
    pub proxy_https: bool,
    pub max_hops: u8,
    /// Allowed domain suffixes for hostname registration. Always contains
    /// `localhost` as the first entry.
    pub domains: Vec<String>,
    /// Per-domain URL display overrides.
    pub domain_displays: Vec<DomainDisplay>,
    /// IP address the proxy listens on (default: 127.0.0.1).
    pub proxy_bind: IpAddr,
    pub app_port: Option<u16>,
    pub app_port_range: (u16, u16),
    pub app_force: bool,
    pub ready_timeout: u64,
    pub state_dir: Option<PathBuf>,
}

/// Resolved display override for a single domain suffix.
#[derive(Debug, Clone)]
pub struct DomainDisplay {
    pub suffix: String,
    pub https: bool,
    pub port: Option<u16>,
}

impl DomainDisplay {
    pub fn scheme(&self) -> &'static str {
        if self.https { "https" } else { "http" }
    }

    /// Whether this entry specifies the default port for its scheme.
    pub fn is_default_port(&self) -> bool {
        matches!(
            (self.https, self.port),
            (true, None) | (true, Some(443)) | (false, None) | (false, Some(80))
        )
    }

    /// Format a URL for a given hostname using this display entry.
    pub fn format(&self, hostname: &str) -> String {
        if self.is_default_port() {
            format!("{}://{}", self.scheme(), hostname)
        } else if let Some(port) = self.port {
            format!("{}://{}:{}", self.scheme(), hostname, port)
        } else {
            format!("{}://{}", self.scheme(), hostname)
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            proxy_port: DEFAULT_PORT,
            proxy_https: false,
            max_hops: 5,
            domains: vec!["localhost".to_string()],
            domain_displays: Vec::new(),
            proxy_bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
            app_port: None,
            app_port_range: (3000, 9999),
            app_force: false,
            ready_timeout: 30,
            state_dir: None,
        }
    }
}

impl RawConfig {
    /// Merge another RawConfig on top of self (other wins on conflicts).
    pub fn merge(self, other: RawConfig) -> RawConfig {
        RawConfig {
            proxy: merge_opt(self.proxy, other.proxy, |a, b| RawProxy {
                listen: b.listen.or(a.listen),
                port: b.port.or(a.port),
                https: b.https.or(a.https),
                max_hops: b.max_hops.or(a.max_hops),
                domains: b.domains.or(a.domains),
                display: b.display.or(a.display),
                bind: b.bind.or(a.bind),
            }),
            app: merge_opt(self.app, other.app, |a, b| RawApp {
                port: b.port.or(a.port),
                port_range_start: b.port_range_start.or(a.port_range_start),
                port_range_end: b.port_range_end.or(a.port_range_end),
                force: b.force.or(a.force),
                ready_timeout: b.ready_timeout.or(a.ready_timeout),
            }),
            paths: merge_opt(self.paths, other.paths, |a, b| RawPaths {
                state_dir: b.state_dir.or(a.state_dir),
            }),
        }
    }

    /// Apply env var overrides on top of this config.
    pub fn with_env_overrides(mut self) -> Self {
        if let Ok(val) = std::env::var("NSL_LISTEN") {
            let listen = val.trim().to_string();
            if !listen.is_empty() {
                self.proxy.get_or_insert_with(Default::default).listen = Some(listen);
            }
        }
        if let Ok(val) = std::env::var("NSL_HTTPS") {
            let https = matches!(val.as_str(), "1" | "true" | "yes");
            self.proxy.get_or_insert_with(Default::default).https = Some(https);
        }
        if let Ok(val) = std::env::var("NSL_DOMAINS") {
            let domains: Vec<String> = val
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if !domains.is_empty() {
                self.proxy.get_or_insert_with(Default::default).domains = Some(domains);
            }
        }
        if let Ok(val) = std::env::var("NSL_STATE_DIR") {
            self.paths.get_or_insert_with(Default::default).state_dir = Some(val);
        }
        self
    }

    /// Resolve into a Config with defaults applied.
    pub fn resolve(self) -> Config {
        let defaults = Config::default();
        let (proxy_bind, proxy_port) = resolve_proxy_listen(self.proxy.as_ref(), &defaults);

        Config {
            proxy_port,
            proxy_https: self
                .proxy
                .as_ref()
                .and_then(|p| p.https)
                .unwrap_or(defaults.proxy_https),
            max_hops: self
                .proxy
                .as_ref()
                .and_then(|p| p.max_hops)
                .unwrap_or(defaults.max_hops),
            domains: ensure_localhost(
                self.proxy
                    .as_ref()
                    .and_then(|p| p.domains.clone())
                    .unwrap_or(defaults.domains),
            ),
            domain_displays: self
                .proxy
                .as_ref()
                .and_then(|p| p.display.clone())
                .map(resolve_domain_displays)
                .unwrap_or_default(),
            proxy_bind,
            app_port: self.app.as_ref().and_then(|a| a.port),
            app_port_range: (
                self.app
                    .as_ref()
                    .and_then(|a| a.port_range_start)
                    .unwrap_or(defaults.app_port_range.0),
                self.app
                    .as_ref()
                    .and_then(|a| a.port_range_end)
                    .unwrap_or(defaults.app_port_range.1),
            ),
            app_force: self
                .app
                .as_ref()
                .and_then(|a| a.force)
                .unwrap_or(defaults.app_force),
            ready_timeout: self
                .app
                .as_ref()
                .and_then(|a| a.ready_timeout)
                .unwrap_or(defaults.ready_timeout),
            state_dir: self
                .paths
                .as_ref()
                .and_then(|p| p.state_dir.as_ref())
                .map(PathBuf::from),
        }
    }
}

fn resolve_proxy_listen(proxy: Option<&RawProxy>, defaults: &Config) -> (IpAddr, u16) {
    if let Some(raw) = proxy.and_then(|p| p.listen.as_deref()) {
        match parse_listen(raw) {
            Ok(listen) => return listen,
            Err(e) => {
                let msg = format!("invalid proxy.listen '{}': {} -- using default", raw, e);
                tracing::warn!("{msg}");
                eprintln!("warning: {msg}");
                return (defaults.proxy_bind, defaults.proxy_port);
            }
        }
    }

    let bind = proxy
        .and_then(|p| p.bind.as_deref())
        .and_then(|s| match s.parse::<IpAddr>() {
            Ok(ip) => Some(ip),
            Err(e) => {
                let msg = format!(
                    "invalid deprecated proxy.bind '{}': {} -- using default",
                    s, e
                );
                tracing::warn!("{msg}");
                eprintln!("warning: {msg}");
                None
            }
        })
        .unwrap_or(defaults.proxy_bind);
    let port = proxy.and_then(|p| p.port).unwrap_or(defaults.proxy_port);

    (bind, port)
}

pub fn parse_listen(raw: &str) -> Result<(IpAddr, u16), String> {
    let listen = raw.trim();
    if listen.is_empty() {
        return Err("empty listen address".to_string());
    }

    if let Some(port) = listen.strip_prefix(':') {
        let port = port
            .parse::<u16>()
            .map_err(|e| format!("invalid port: {e}"))?;
        return Ok((IpAddr::V4(Ipv4Addr::UNSPECIFIED), port));
    }

    let addr = listen
        .parse::<SocketAddr>()
        .map_err(|e| format!("expected HOST:PORT or :PORT: {e}"))?;
    Ok((addr.ip(), addr.port()))
}

/// Ensure `localhost` is present in the domains list, preserving user order
/// otherwise. `localhost` is prepended if missing so it takes precedence when
/// iterating.
fn ensure_localhost(mut domains: Vec<String>) -> Vec<String> {
    let has_localhost = domains.iter().any(|d| d == "localhost");
    if !has_localhost {
        domains.insert(0, "localhost".to_string());
    }
    domains
}

/// Normalize raw domain display entries: lowercase suffixes, default to
/// HTTPS, drop entries with empty suffix.
fn resolve_domain_displays(raw: BTreeMap<String, RawDomainDisplay>) -> Vec<DomainDisplay> {
    raw.into_iter()
        .filter_map(|(key, cfg)| {
            let suffix = key.trim().to_lowercase();
            if suffix.is_empty() {
                return None;
            }
            Some(DomainDisplay {
                suffix,
                https: cfg.https.unwrap_or(true),
                port: cfg.port,
            })
        })
        .collect()
}

fn merge_opt<T>(a: Option<T>, b: Option<T>, f: impl FnOnce(T, T) -> T) -> Option<T> {
    match (a, b) {
        (Some(a), Some(b)) => Some(f(a, b)),
        (None, b) => b,
        (a, None) => a,
    }
}

/// Config file search locations (lowest to highest priority).
fn config_file_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    paths.push(PathBuf::from("/etc/nsl/config.toml"));
    paths.push(dirs_or_home().join(".nsl").join("config.toml"));
    if let Ok(cwd) = std::env::current_dir()
        && let Some(project_config) = find_project_config(&cwd)
    {
        paths.push(project_config);
    }
    paths
}

/// Walk up from `start` looking for `nsl.toml`.
fn find_project_config(start: &Path) -> Option<PathBuf> {
    let mut dir = start;
    loop {
        let candidate = dir.join("nsl.toml");
        if candidate.is_file() {
            return Some(candidate);
        }
        dir = dir.parent()?;
    }
}

/// Load a single TOML config file, returning None if missing or unparseable.
fn load_config_file(path: &Path) -> Option<RawConfig> {
    let content = fs::read_to_string(path).ok()?;
    match toml::from_str(&content) {
        Ok(config) => Some(config),
        Err(e) => {
            let msg = format!("failed to parse config {}: {}", path.display(), e);
            tracing::warn!("{msg}");
            eprintln!("warning: {msg}");
            None
        }
    }
}

/// Load the fully resolved config by merging all layers.
pub fn load_config() -> Config {
    let mut merged = RawConfig::default();
    for path in config_file_paths() {
        if let Some(raw) = load_config_file(&path) {
            tracing::debug!("loaded config from {}", path.display());
            merged = merged.merge(raw);
        }
    }
    merged = merged.with_env_overrides();
    merged.resolve()
}

impl Config {
    pub fn proxy_listen(&self) -> String {
        SocketAddr::new(self.proxy_bind, self.proxy_port).to_string()
    }

    pub fn resolve_state_dir(&self) -> PathBuf {
        if let Some(ref dir) = self.state_dir {
            return dir.clone();
        }
        if self.proxy_port < PRIVILEGED_PORT_THRESHOLD {
            PathBuf::from("/tmp/nsl")
        } else {
            dirs_or_home().join(".nsl")
        }
    }
}

fn dirs_or_home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
}

/// Print current resolved config to stdout.
pub fn print_config() {
    let config = load_config();
    println!("Config");
    println!("  proxy.listen:     {}", config.proxy_listen());
    println!("  proxy.https:      {}", config.proxy_https);
    println!("  proxy.max_hops:   {}", config.max_hops);
    println!("  proxy.domains:    {}", config.domains.join(", "));
    if !config.domain_displays.is_empty() {
        println!("  proxy.display:");
        for d in &config.domain_displays {
            let port = match d.port {
                Some(p) => format!(":{}", p),
                None => String::new(),
            };
            println!("    - {} -> {}://{{host}}{}", d.suffix, d.scheme(), port);
        }
    }
    if let Some(port) = config.app_port {
        println!("  app.port:         {}", port);
    }
    println!(
        "  app.port_range:   {}-{}",
        config.app_port_range.0, config.app_port_range.1
    );
    println!("  app.force:        {}", config.app_force);
    println!("  app.ready_timeout: {}s", config.ready_timeout);
    println!(
        "  state_dir:        {}",
        config.resolve_state_dir().display()
    );

    println!();
    println!("Sources (lowest to highest priority):");
    for path in config_file_paths() {
        let exists = path.is_file();
        let marker = if exists { "+" } else { "-" };
        println!("  [{}] {}", marker, path.display());
    }
    println!("  [*] environment variables");
    println!("  [*] CLI flags");
}

#[cfg(test)]
mod tests;

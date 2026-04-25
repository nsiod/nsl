use std::path::PathBuf;

pub use crate::platform::fix_ownership;

/// Default proxy port.
pub const DEFAULT_PORT: u16 = 3355;

/// Threshold below which ports require elevated privileges.
pub const PRIVILEGED_PORT_THRESHOLD: u16 = 1024;

/// Resolve the state directory based on the proxy port.
///
/// - Port < 1024: platform-specific privileged dir (e.g. `/tmp/nsl` on Unix,
///   `%LOCALAPPDATA%\nsl` on Windows)
/// - Port >= 1024: `~/.nsl` (user-scoped)
pub fn resolve_state_dir(port: u16) -> PathBuf {
    if let Ok(dir) = std::env::var("NSL_STATE_DIR") {
        return PathBuf::from(dir);
    }

    if port < PRIVILEGED_PORT_THRESHOLD {
        crate::platform::privileged_state_dir()
    } else {
        crate::platform::user_home().join(".nsl")
    }
}

/// Sanitize a string for use as a .localhost hostname label.
pub fn sanitize_for_hostname(name: &str) -> String {
    let sanitized: String = name
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();

    // Collapse consecutive hyphens and trim
    let collapsed = sanitized
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");

    truncate_label(&collapsed)
}

/// Truncate a DNS label to 63 characters (RFC 1035).
fn truncate_label(label: &str) -> String {
    if label.len() <= 63 {
        return label.to_string();
    }

    use std::fmt::Write;
    let hash = {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        label.hash(&mut hasher);
        let h = hasher.finish();
        let mut buf = String::new();
        write!(buf, "{:06x}", h & 0xFFFFFF).unwrap();
        buf
    };

    let max_prefix = 63 - 7; // "-" + 6-char hash
    let prefix = &label[..max_prefix].trim_end_matches('-');
    format!("{}-{}", prefix, hash)
}

/// Split a `name[:path]` input into (name, path).
///
/// The `:` character separates the app name from an optional path prefix.
/// Paths are normalized to start with `/`; an empty or missing path becomes
/// the root `/`.
///
/// Examples:
/// - `myapp`              -> (`myapp`, `/`)
/// - `myapp:/api`         -> (`myapp`, `/api`)
/// - `myapp:api`          -> (`myapp`, `/api`)
/// - `myapp:/api/v1`      -> (`myapp`, `/api/v1`)
/// - `myapp.localhost:/x` -> (`myapp.localhost`, `/x`)
/// - `myapp:`             -> (`myapp`, `/`)
pub fn split_name_path(raw: &str) -> (String, String) {
    let cleaned = raw.trim();
    match cleaned.split_once(':') {
        Some((name, rest)) => {
            let trimmed = rest.trim_start_matches('/');
            let path = if trimmed.is_empty() {
                "/".to_string()
            } else {
                format!("/{}", trimmed)
            };
            (name.to_string(), path)
        }
        None => (cleaned.to_string(), "/".to_string()),
    }
}

/// Parse and normalize a hostname, appending the first configured domain if needed.
///
/// `domains` is the list of allowed domain suffixes (e.g. `["localhost", "dev.local"]`).
/// If the input already ends with one of the domains, it is accepted as-is.
/// Otherwise, the first domain in the list is appended as suffix.
pub fn parse_hostname(input: &str, domains: &[String]) -> Result<String, String> {
    let hostname = input
        .trim()
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .split('/')
        .next()
        .unwrap_or("")
        .to_lowercase();

    if hostname.is_empty() {
        return Err("Hostname cannot be empty".to_string());
    }

    let default_domain = domains.first().map(|s| s.as_str()).unwrap_or("localhost");

    // Check if hostname already ends with any configured domain
    let has_domain = domains
        .iter()
        .any(|d| hostname == *d || hostname.ends_with(&format!(".{}", d)));

    let hostname = if has_domain {
        hostname
    } else {
        format!("{}.{}", hostname, default_domain)
    };

    // Extract the name part (before the domain suffix) for validation
    let name = domains
        .iter()
        .find_map(|d| hostname.strip_suffix(&format!(".{}", d)))
        .or_else(|| {
            domains
                .iter()
                .find_map(|d| if hostname == *d { Some("") } else { None })
        })
        .unwrap_or(&hostname);

    if name.is_empty() {
        return Err("Hostname cannot be empty".to_string());
    }

    if name.contains("..") {
        return Err(format!(
            "Invalid hostname \"{}\": consecutive dots are not allowed",
            name
        ));
    }

    Ok(hostname)
}

/// Extract the hostname prefix (label before the domain suffix).
///
/// For example, given hostname "myapp.localhost" and domains ["localhost", "dev.local"],
/// returns "myapp".
pub fn extract_hostname_prefix(hostname: &str, domains: &[String]) -> String {
    for domain in domains {
        if let Some(prefix) = hostname.strip_suffix(&format!(".{}", domain)) {
            return prefix.to_string();
        }
    }
    hostname.to_string()
}

/// Generate URLs for all configured domains from a hostname prefix.
///
/// Given a prefix like "myapp" and domains ["localhost", "dev.local"],
/// returns URLs for "myapp.localhost" and "myapp.dev.local".
///
/// `domain_displays` lets external domains be rendered via a reverse-proxy
/// URL (e.g. `https://myapp.example.com`) instead of the local proxy address.
pub fn format_urls(
    hostname_prefix: &str,
    domains: &[String],
    proxy_port: u16,
    tls: bool,
    domain_displays: &[crate::config::DomainDisplay],
) -> Vec<String> {
    domains
        .iter()
        .map(|domain| {
            let full_hostname = format!("{}.{}", hostname_prefix, domain);
            format_url(&full_hostname, proxy_port, tls, domain_displays)
        })
        .collect()
}

/// Format a URL for the given hostname.
///
/// If `domain_displays` contains a matching suffix (longest match wins), that
/// entry's scheme/port are used. Otherwise the local proxy address is used
/// (`{scheme}://hostname[:proxy_port]`).
pub fn format_url(
    hostname: &str,
    proxy_port: u16,
    tls: bool,
    domain_displays: &[crate::config::DomainDisplay],
) -> String {
    if let Some(display) = match_display(hostname, domain_displays) {
        return display.format(hostname);
    }

    let proto = if tls { "https" } else { "http" };
    let default_port = if tls { 443 } else { 80 };
    if proxy_port == default_port {
        format!("{}://{}", proto, hostname)
    } else {
        format!("{}://{}:{}", proto, hostname, proxy_port)
    }
}

/// Find the most specific (longest) matching display entry for a hostname.
fn match_display<'a>(
    hostname: &str,
    displays: &'a [crate::config::DomainDisplay],
) -> Option<&'a crate::config::DomainDisplay> {
    let host_lower = hostname.to_lowercase();
    displays
        .iter()
        .filter(|d| host_lower == d.suffix || host_lower.ends_with(&format!(".{}", d.suffix)))
        .max_by_key(|d| d.suffix.len())
}

#[cfg(test)]
mod tests;

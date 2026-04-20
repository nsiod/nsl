use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::config::DomainDisplay;
use crate::utils::{DEFAULT_PORT, format_url, resolve_state_dir};

/// Information about the proxy process.
#[derive(Debug)]
pub struct ProxyStatus {
    pub running: bool,
    pub pid: Option<u32>,
    pub port: u16,
    pub tls: bool,
    pub state_dir: PathBuf,
    pub uptime_secs: Option<u64>,
}

/// Extended information about a route's process.
#[derive(Debug)]
pub struct RouteStatus {
    pub hostname: String,
    pub port: u16,
    pub pid: u32,
    pub url: String,
    pub kind: RouteKind,
    pub alive: bool,
    pub process_name: Option<String>,
}

#[derive(Debug, PartialEq)]
pub enum RouteKind {
    /// Static route (pid=0), registered via `nsl route`
    Static,
    /// Dynamic route registered by a running process
    Dynamic,
}

/// Read the proxy PID from the state directory.
fn read_proxy_pid(state_dir: &Path) -> Option<u32> {
    let pid_path = state_dir.join("proxy.pid");
    fs::read_to_string(pid_path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

/// Read the proxy port from the state directory.
fn read_proxy_port(state_dir: &Path) -> Option<u16> {
    let port_path = state_dir.join("proxy.port");
    fs::read_to_string(port_path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

/// Check if TLS marker exists in the state directory.
fn read_tls_marker(state_dir: &Path) -> bool {
    state_dir.join("proxy.tls").exists()
}

/// Check if a process is alive.
fn is_alive(pid: u32) -> bool {
    crate::platform::is_process_alive(pid)
}

/// Get the uptime in seconds from a PID file's mtime.
fn uptime_from_pid_file(state_dir: &Path) -> Option<u64> {
    let pid_path = state_dir.join("proxy.pid");
    let meta = fs::metadata(pid_path).ok()?;
    let modified = meta.modified().ok()?;
    SystemTime::now()
        .duration_since(modified)
        .ok()
        .map(|d| d.as_secs())
}

/// Try to get the process command name via /proc/<pid>/comm (Linux).
fn get_process_name(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }
    let comm_path = format!("/proc/{}/comm", pid);
    fs::read_to_string(comm_path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Discover which state directory has an active proxy.
/// Checks user dir first, then system dir.
fn discover_active_state_dir() -> (PathBuf, u16) {
    if let Ok(dir) = std::env::var("NSL_STATE_DIR") {
        let dir = PathBuf::from(dir);
        let port = read_proxy_port(&dir).unwrap_or(DEFAULT_PORT);
        return (dir, port);
    }

    // Check user dir
    let user_dir = resolve_state_dir(DEFAULT_PORT);
    if let Some(port) = read_proxy_port(&user_dir)
        && let Some(pid) = read_proxy_pid(&user_dir)
        && is_alive(pid)
    {
        return (user_dir, port);
    }

    // Check system dir
    let sys_dir = resolve_state_dir(80);
    if let Some(port) = read_proxy_port(&sys_dir)
        && let Some(pid) = read_proxy_pid(&sys_dir)
        && is_alive(pid)
    {
        return (sys_dir, port);
    }

    // Nothing running, return default
    (resolve_state_dir(DEFAULT_PORT), DEFAULT_PORT)
}

/// Get the proxy status.
pub fn get_proxy_status() -> ProxyStatus {
    let (state_dir, port) = discover_active_state_dir();
    let pid = read_proxy_pid(&state_dir);
    let tls = read_tls_marker(&state_dir);
    let running = pid.is_some_and(is_alive);
    let uptime_secs = if running {
        uptime_from_pid_file(&state_dir)
    } else {
        None
    };

    ProxyStatus {
        running,
        pid: if running { pid } else { None },
        port,
        tls,
        state_dir,
        uptime_secs,
    }
}

/// Get the status of all routes.
pub fn get_route_statuses(
    state_dir: &Path,
    proxy_port: u16,
    tls: bool,
    domain_displays: &[DomainDisplay],
) -> Vec<RouteStatus> {
    // Load raw routes without filtering dead processes (we want to show them)
    let routes = match fs::read_to_string(state_dir.join("routes.json")) {
        Ok(raw) => match serde_json::from_str::<Vec<crate::routes::RouteMapping>>(&raw) {
            Ok(routes) => routes,
            Err(e) => {
                eprintln!("warning: failed to parse routes.json: {e}");
                Vec::new()
            }
        },
        Err(_) => return Vec::new(),
    };

    routes
        .into_iter()
        .map(|r| {
            let alive = is_alive(r.pid);
            let kind = if r.pid == 0 {
                RouteKind::Static
            } else {
                RouteKind::Dynamic
            };
            let url = format_url(&r.hostname, proxy_port, tls, domain_displays);
            let process_name = get_process_name(r.pid);

            RouteStatus {
                hostname: r.hostname,
                port: r.port,
                pid: r.pid,
                url,
                kind,
                alive,
                process_name,
            }
        })
        .collect()
}

/// Format uptime as human-readable string.
pub fn format_uptime(secs: u64) -> String {
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else if secs < 86400 {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{}d {}h", secs / 86400, (secs % 86400) / 3600)
    }
}

/// Print full status report to stdout.
pub fn print_status() {
    let proxy = get_proxy_status();

    // -- Proxy section --
    println!("Proxy");
    if proxy.running {
        let pid = proxy.pid.unwrap_or(0);
        let proto = if proxy.tls { "HTTPS/2" } else { "HTTP" };
        let uptime = proxy
            .uptime_secs
            .map(|s| format!(" (uptime {})", format_uptime(s)))
            .unwrap_or_default();
        println!("  status:    running{}", uptime);
        println!("  pid:       {}", pid);
        println!("  port:      {}", proxy.port);
        println!("  protocol:  {}", proto);
    } else {
        println!("  status:    stopped");
        println!("  port:      {} (default)", proxy.port);
    }
    println!("  state dir: {}", proxy.state_dir.display());

    // -- Routes section --
    let config = crate::config::load_config();
    let routes = get_route_statuses(
        &proxy.state_dir,
        proxy.port,
        proxy.tls,
        &config.domain_displays,
    );
    println!();
    println!("Routes ({})", routes.len());

    if routes.is_empty() {
        println!("  (none)");
        return;
    }

    // Calculate column widths
    let max_host = routes.iter().map(|r| r.hostname.len()).max().unwrap_or(0);

    for route in &routes {
        let status_marker = if route.alive { "+" } else { "-" };
        let kind_label = match route.kind {
            RouteKind::Static => "static".to_string(),
            RouteKind::Dynamic => {
                let name = route.process_name.as_deref().unwrap_or("?");
                format!("pid {} ({})", route.pid, name)
            }
        };
        let alive_label = if route.alive { "" } else { " [dead]" };

        println!(
            "  [{}] {:<width_h$}  ->  localhost:{:<5}  {}{}",
            status_marker,
            route.hostname,
            route.port,
            kind_label,
            alive_label,
            width_h = max_host,
        );
        println!("      {}", route.url,);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_uptime() {
        assert_eq!(format_uptime(30), "30s");
        assert_eq!(format_uptime(90), "1m 30s");
        assert_eq!(format_uptime(3661), "1h 1m");
        assert_eq!(format_uptime(90000), "1d 1h");
    }

    #[test]
    fn test_route_kind() {
        assert_eq!(RouteKind::Static, RouteKind::Static);
        assert_ne!(RouteKind::Static, RouteKind::Dynamic);
    }

    #[test]
    fn test_read_proxy_pid_missing() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert!(read_proxy_pid(tmp.path()).is_none());
    }

    #[test]
    fn test_read_proxy_pid_valid() {
        let tmp = tempfile::TempDir::new().unwrap();
        fs::write(tmp.path().join("proxy.pid"), "12345").unwrap();
        assert_eq!(read_proxy_pid(tmp.path()), Some(12345));
    }

    #[test]
    fn test_read_proxy_port_missing() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert!(read_proxy_port(tmp.path()).is_none());
    }

    #[test]
    fn test_read_proxy_port_valid() {
        let tmp = tempfile::TempDir::new().unwrap();
        fs::write(tmp.path().join("proxy.port"), "1355").unwrap();
        assert_eq!(read_proxy_port(tmp.path()), Some(1355));
    }

    #[test]
    fn test_tls_marker() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert!(!read_tls_marker(tmp.path()));
        fs::write(tmp.path().join("proxy.tls"), "1").unwrap();
        assert!(read_tls_marker(tmp.path()));
    }

    #[test]
    fn test_get_route_statuses_empty() {
        let tmp = tempfile::TempDir::new().unwrap();
        let routes = get_route_statuses(tmp.path(), 1355, false, &[]);
        assert!(routes.is_empty());
    }

    #[test]
    fn test_get_route_statuses_with_routes() {
        let tmp = tempfile::TempDir::new().unwrap();
        let routes_json = serde_json::json!([
            {"hostname": "myapp.localhost", "port": 4000, "pid": 0},
            {"hostname": "api.localhost", "port": 4001, "pid": 99999}
        ]);
        fs::write(
            tmp.path().join("routes.json"),
            serde_json::to_string(&routes_json).unwrap(),
        )
        .unwrap();

        let statuses = get_route_statuses(tmp.path(), 1355, false, &[]);
        assert_eq!(statuses.len(), 2);

        // pid 0 = static, always alive
        assert_eq!(statuses[0].kind, RouteKind::Static);
        assert!(statuses[0].alive);
        assert_eq!(statuses[0].url, "http://myapp.localhost:1355");

        // pid 99999 = dynamic, likely dead
        assert_eq!(statuses[1].kind, RouteKind::Dynamic);
        assert!(!statuses[1].alive); // process 99999 should not exist
    }
}

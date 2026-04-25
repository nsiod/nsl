use super::*;
use std::collections::BTreeMap;

#[test]
fn test_default_config() {
    let config = Config::default();
    assert_eq!(config.proxy_port, 3355);
    assert!(!config.proxy_https);
    assert_eq!(config.max_hops, 5);
    assert_eq!(config.domains, vec!["localhost".to_string()]);
    assert_eq!(config.app_port_range, (20000, 29999));
    assert!(!config.app_force);
    assert!(config.state_dir.is_none());
}

#[test]
fn test_raw_config_resolve_defaults() {
    let raw = RawConfig::default();
    let config = raw.resolve();
    assert_eq!(config.proxy_port, 3355);
    assert!(!config.proxy_https);
}

#[test]
fn test_raw_config_resolve_overrides() {
    let raw = RawConfig {
        proxy: Some(RawProxy {
            listen: Some("127.0.0.1:8080".to_string()),
            https: Some(true),
            ..Default::default()
        }),
        ..Default::default()
    };
    let config = raw.resolve();
    assert_eq!(config.proxy_port, 8080);
    assert!(config.proxy_https);
    assert_eq!(config.max_hops, 5); // default
}

#[test]
fn test_merge_both_present() {
    let base = RawConfig {
        proxy: Some(RawProxy {
            listen: Some("127.0.0.1:1355".to_string()),
            https: Some(false),
            max_hops: Some(3),
            ..Default::default()
        }),
        ..Default::default()
    };
    let overlay = RawConfig {
        proxy: Some(RawProxy {
            listen: Some("127.0.0.1:8080".to_string()),
            ..Default::default()
        }),
        app: Some(RawApp {
            force: Some(true),
            ..Default::default()
        }),
        ..Default::default()
    };
    let merged = base.merge(overlay).resolve();
    assert_eq!(merged.proxy_port, 8080); // overlay wins
    assert!(!merged.proxy_https); // base kept
    assert_eq!(merged.max_hops, 3); // base kept
    assert!(merged.app_force); // overlay added
}

#[test]
fn test_merge_only_base() {
    let base = RawConfig {
        proxy: Some(RawProxy {
            listen: Some("127.0.0.1:9090".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let merged = base.merge(RawConfig::default()).resolve();
    assert_eq!(merged.proxy_port, 9090);
    assert_eq!(merged.domains, vec!["localhost".to_string()]); // default preserved
}

#[test]
fn test_merge_only_overlay() {
    let overlay = RawConfig {
        app: Some(RawApp {
            force: Some(true),
            ..Default::default()
        }),
        ..Default::default()
    };
    let merged = RawConfig::default().merge(overlay).resolve();
    assert!(merged.app_force);
}

#[test]
fn test_load_config_file_missing() {
    let result = load_config_file(Path::new("/nonexistent/config.toml"));
    assert!(result.is_none());
}

#[test]
fn test_load_config_file_valid() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config_path = tmp.path().join("config.toml");
    fs::write(
        &config_path,
        r#"
[proxy]
listen = "127.0.0.1:4000"
https = true

[app]
force = true
"#,
    )
    .unwrap();

    let raw = load_config_file(&config_path).unwrap();
    let config = raw.resolve();
    assert_eq!(config.proxy_port, 4000);
    assert!(config.proxy_https);
    assert!(config.app_force);
}

#[test]
fn test_load_config_file_invalid_toml() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config_path = tmp.path().join("config.toml");
    fs::write(&config_path, "not valid [[[ toml").unwrap();

    let result = load_config_file(&config_path);
    assert!(result.is_none());
}

#[test]
fn test_find_project_config() {
    let tmp = tempfile::TempDir::new().unwrap();
    let sub = tmp.path().join("a").join("b");
    fs::create_dir_all(&sub).unwrap();
    fs::write(
        tmp.path().join("nsl.toml"),
        "[proxy]\nlisten = \"127.0.0.1:7777\"\n",
    )
    .unwrap();

    let found = find_project_config(&sub);
    assert_eq!(found.unwrap(), tmp.path().join("nsl.toml"));
}

#[test]
fn test_find_project_config_not_found() {
    let tmp = tempfile::TempDir::new().unwrap();
    let found = find_project_config(tmp.path());
    assert!(found.is_none());
}

#[test]
fn test_config_resolve_state_dir_explicit() {
    let config = Config {
        state_dir: Some(PathBuf::from("/custom/state")),
        ..Default::default()
    };
    assert_eq!(config.resolve_state_dir(), PathBuf::from("/custom/state"));
}

#[test]
fn test_custom_domains_config() {
    let raw = RawConfig {
        proxy: Some(RawProxy {
            domains: Some(vec!["dev.local".to_string(), "test".to_string()]),
            ..Default::default()
        }),
        ..Default::default()
    };
    let config = raw.resolve();
    // `localhost` is always implicitly included (prepended if user omitted it).
    assert_eq!(
        config.domains,
        vec![
            "localhost".to_string(),
            "dev.local".to_string(),
            "test".to_string(),
        ]
    );
}

#[test]
fn test_localhost_always_present_even_if_user_omits() {
    let raw = RawConfig {
        proxy: Some(RawProxy {
            domains: Some(vec!["myapp.com".to_string()]),
            ..Default::default()
        }),
        ..Default::default()
    };
    let config = raw.resolve();
    assert!(config.domains.contains(&"localhost".to_string()));
    assert!(config.domains.contains(&"myapp.com".to_string()));
}

#[test]
fn test_localhost_not_duplicated_if_user_included_it() {
    let raw = RawConfig {
        proxy: Some(RawProxy {
            domains: Some(vec!["localhost".to_string(), "myapp.com".to_string()]),
            ..Default::default()
        }),
        ..Default::default()
    };
    let config = raw.resolve();
    let localhost_count = config.domains.iter().filter(|d| *d == "localhost").count();
    assert_eq!(localhost_count, 1);
}

#[test]
fn test_domain_display_resolves_defaults() {
    let mut display = BTreeMap::new();
    display.insert(
        "myapp.com".to_string(),
        RawDomainDisplay {
            https: None,
            port: None,
        },
    );
    let raw = RawConfig {
        proxy: Some(RawProxy {
            domains: Some(vec!["myapp.com".to_string()]),
            display: Some(display),
            ..Default::default()
        }),
        ..Default::default()
    };
    let config = raw.resolve();
    assert_eq!(config.domain_displays.len(), 1);
    assert_eq!(config.domain_displays[0].suffix, "myapp.com");
    assert!(config.domain_displays[0].https);
    assert!(config.domain_displays[0].port.is_none());
}

#[test]
fn test_domain_display_from_toml() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config_path = tmp.path().join("config.toml");
    fs::write(
        &config_path,
        r#"
[proxy]
domains = ["myapp.com"]

[proxy.display."myapp.com"]
https = true

[proxy.display."dev.internal"]
https = false
port = 8080
"#,
    )
    .unwrap();

    let raw = load_config_file(&config_path).unwrap();
    let config = raw.resolve();
    assert_eq!(config.domain_displays.len(), 2);
    // BTreeMap orders keys alphabetically: "dev.internal" < "myapp.com"
    let dev = config
        .domain_displays
        .iter()
        .find(|d| d.suffix == "dev.internal")
        .unwrap();
    assert!(!dev.https);
    assert_eq!(dev.port, Some(8080));
    let myapp = config
        .domain_displays
        .iter()
        .find(|d| d.suffix == "myapp.com")
        .unwrap();
    assert!(myapp.https);
    assert!(myapp.port.is_none());
}

#[test]
fn test_domains_merge_overlay_wins() {
    let base = RawConfig {
        proxy: Some(RawProxy {
            domains: Some(vec!["localhost".to_string()]),
            ..Default::default()
        }),
        ..Default::default()
    };
    let overlay = RawConfig {
        proxy: Some(RawProxy {
            domains: Some(vec!["dev.local".to_string(), "localhost".to_string()]),
            ..Default::default()
        }),
        ..Default::default()
    };
    let config = base.merge(overlay).resolve();
    assert_eq!(
        config.domains,
        vec!["dev.local".to_string(), "localhost".to_string()]
    );
}

#[test]
fn test_load_config_file_with_domains() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config_path = tmp.path().join("config.toml");
    fs::write(
        &config_path,
        r#"
[proxy]
listen = "127.0.0.1:1355"
domains = ["dev.local", "localhost", "test"]
"#,
    )
    .unwrap();

    let raw = load_config_file(&config_path).unwrap();
    let config = raw.resolve();
    assert_eq!(
        config.domains,
        vec![
            "dev.local".to_string(),
            "localhost".to_string(),
            "test".to_string(),
        ]
    );
}

#[test]
fn test_default_listen_is_loopback() {
    let config = Config::default();
    assert_eq!(config.proxy_bind, IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)));
    assert_eq!(config.proxy_port, 3355);
    assert_eq!(config.proxy_listen(), "127.0.0.1:3355");
}

#[test]
fn test_listen_config_any_address() {
    let raw = RawConfig {
        proxy: Some(RawProxy {
            listen: Some(":8080".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let config = raw.resolve();
    assert_eq!(config.proxy_bind, IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)));
    assert_eq!(config.proxy_port, 8080);
}

#[test]
fn test_parse_listen_colon_port() {
    let (bind, port) = parse_listen(":1355").unwrap();
    assert_eq!(bind, IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)));
    assert_eq!(port, 1355);
}

#[test]
fn test_parse_listen_host_port() {
    let (bind, port) = parse_listen("127.0.0.1:1355").unwrap();
    assert_eq!(bind, IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)));
    assert_eq!(port, 1355);
}

#[test]
fn test_listen_config_with_host() {
    let raw = RawConfig {
        proxy: Some(RawProxy {
            listen: Some("127.0.0.1:8080".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let config = raw.resolve();
    assert_eq!(config.proxy_bind, IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)));
    assert_eq!(config.proxy_port, 8080);
}

#[test]
fn test_listen_invalid_falls_back_to_default() {
    let raw = RawConfig {
        proxy: Some(RawProxy {
            listen: Some("not-a-listen-address".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let config = raw.resolve();
    assert_eq!(config.proxy_bind, IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)));
    assert_eq!(config.proxy_port, 3355);
}

#[test]
fn test_deprecated_port_and_bind_still_resolve() {
    let raw = RawConfig {
        proxy: Some(RawProxy {
            port: Some(8080),
            bind: Some("0.0.0.0".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let config = raw.resolve();
    assert_eq!(config.proxy_bind, IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)));
    assert_eq!(config.proxy_port, 8080);
}

#[test]
fn test_config_resolve_state_dir_privileged_port() {
    let config = Config {
        proxy_port: 80,
        state_dir: None,
        ..Default::default()
    };
    assert_eq!(config.resolve_state_dir(), PathBuf::from("/tmp/nsl"));
}

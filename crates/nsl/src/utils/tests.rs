#![allow(unsafe_code)]

use super::*;
#[cfg(unix)]
use std::path::Path;
use std::path::PathBuf;

#[test]
fn test_sanitize_for_hostname() {
    assert_eq!(sanitize_for_hostname("MyApp"), "myapp");
    assert_eq!(sanitize_for_hostname("my_app"), "my-app");
    assert_eq!(sanitize_for_hostname("@scope/pkg"), "scope-pkg");
    assert_eq!(sanitize_for_hostname("--hello--"), "hello");
}

#[test]
fn test_split_name_path() {
    // No colon -> whole input is the name, path defaults to "/"
    assert_eq!(split_name_path("myapp"), ("myapp".into(), "/".into()));
    assert_eq!(
        split_name_path("myapp.localhost"),
        ("myapp.localhost".into(), "/".into())
    );

    // Colon separates name from path; leading slash is optional
    assert_eq!(
        split_name_path("myapp:/api"),
        ("myapp".into(), "/api".into())
    );
    assert_eq!(
        split_name_path("myapp:api"),
        ("myapp".into(), "/api".into())
    );
    assert_eq!(
        split_name_path("myapp:/api/v1"),
        ("myapp".into(), "/api/v1".into())
    );
    assert_eq!(
        split_name_path("myapp.localhost:/x"),
        ("myapp.localhost".into(), "/x".into())
    );

    // Empty path after colon normalizes to root
    assert_eq!(split_name_path("myapp:"), ("myapp".into(), "/".into()));
    assert_eq!(split_name_path("myapp:/"), ("myapp".into(), "/".into()));

    // Whitespace is trimmed
    assert_eq!(
        split_name_path("  myapp:/api  "),
        ("myapp".into(), "/api".into())
    );
}

#[test]
fn test_parse_hostname() {
    let domains = vec!["localhost".to_string()];
    assert_eq!(
        parse_hostname("myapp", &domains).unwrap(),
        "myapp.localhost"
    );
    assert_eq!(
        parse_hostname("http://myapp.localhost", &domains).unwrap(),
        "myapp.localhost"
    );
    assert!(parse_hostname("", &domains).is_err());
}

#[test]
fn test_parse_hostname_custom_domains() {
    let domains = vec!["dev.local".to_string(), "localhost".to_string()];

    // Default domain is first in list
    assert_eq!(
        parse_hostname("myapp", &domains).unwrap(),
        "myapp.dev.local"
    );

    // Recognizes both configured domains
    assert_eq!(
        parse_hostname("myapp.dev.local", &domains).unwrap(),
        "myapp.dev.local"
    );
    assert_eq!(
        parse_hostname("myapp.localhost", &domains).unwrap(),
        "myapp.localhost"
    );

    // Strips protocol
    assert_eq!(
        parse_hostname("http://api.dev.local", &domains).unwrap(),
        "api.dev.local"
    );
}

#[test]
fn test_parse_hostname_single_custom_domain() {
    let domains = vec!["test".to_string()];
    assert_eq!(parse_hostname("myapp", &domains).unwrap(), "myapp.test");
    assert_eq!(
        parse_hostname("myapp.test", &domains).unwrap(),
        "myapp.test"
    );
}

#[test]
fn test_format_url() {
    assert_eq!(
        format_url("myapp.localhost", 1355, false, &[]),
        "http://myapp.localhost:1355"
    );
    assert_eq!(
        format_url("myapp.localhost", 80, false, &[]),
        "http://myapp.localhost"
    );
    assert_eq!(
        format_url("myapp.localhost", 443, true, &[]),
        "https://myapp.localhost"
    );
}

#[test]
fn test_format_url_with_domain_display_default_https() {
    let displays = vec![crate::config::DomainDisplay {
        suffix: "myapp.com".to_string(),
        https: true,
        port: None,
    }];
    assert_eq!(
        format_url("api.myapp.com", 1355, false, &displays),
        "https://api.myapp.com"
    );
    // Non-matching hostname falls back to proxy_port behavior.
    assert_eq!(
        format_url("api.localhost", 1355, false, &displays),
        "http://api.localhost:1355"
    );
}

#[test]
fn test_format_url_with_domain_display_custom_scheme_port() {
    let displays = vec![crate::config::DomainDisplay {
        suffix: "dev.internal".to_string(),
        https: false,
        port: Some(8080),
    }];
    assert_eq!(
        format_url("api.dev.internal", 1355, false, &displays),
        "http://api.dev.internal:8080"
    );
}

#[test]
fn test_format_url_with_domain_display_longest_match_wins() {
    let displays = vec![
        crate::config::DomainDisplay {
            suffix: "example.com".to_string(),
            https: true,
            port: None,
        },
        crate::config::DomainDisplay {
            suffix: "staging.example.com".to_string(),
            https: false,
            port: Some(8081),
        },
    ];
    assert_eq!(
        format_url("api.staging.example.com", 1355, false, &displays),
        "http://api.staging.example.com:8081"
    );
    assert_eq!(
        format_url("api.example.com", 1355, false, &displays),
        "https://api.example.com"
    );
}

#[test]
fn test_truncate_label() {
    let short = "myapp";
    assert_eq!(truncate_label(short), "myapp");

    let long = "a".repeat(100);
    let truncated = truncate_label(&long);
    assert!(truncated.len() <= 63);
}

#[test]
fn test_extract_hostname_prefix() {
    let domains = vec!["localhost".to_string(), "dev.local".to_string()];
    assert_eq!(
        extract_hostname_prefix("myapp.localhost", &domains),
        "myapp"
    );
    assert_eq!(
        extract_hostname_prefix("myapp.dev.local", &domains),
        "myapp"
    );
    assert_eq!(
        extract_hostname_prefix("myapp.unknown", &domains),
        "myapp.unknown"
    );
}

#[test]
fn test_format_urls_single_domain() {
    let domains = vec!["localhost".to_string()];
    let urls = format_urls("myapp", &domains, 1355, false, &[]);
    assert_eq!(urls, vec!["http://myapp.localhost:1355"]);
}

#[test]
fn test_format_urls_multiple_domains() {
    let domains = vec!["localhost".to_string(), "dev.local".to_string()];
    let urls = format_urls("myapp", &domains, 1355, false, &[]);
    assert_eq!(
        urls,
        vec!["http://myapp.localhost:1355", "http://myapp.dev.local:1355",]
    );
}

#[test]
fn test_format_urls_default_port() {
    let domains = vec!["localhost".to_string()];
    let urls = format_urls("myapp", &domains, 80, false, &[]);
    assert_eq!(urls, vec!["http://myapp.localhost"]);
}

#[test]
fn test_format_urls_tls() {
    let domains = vec!["localhost".to_string()];
    let urls = format_urls("myapp", &domains, 443, true, &[]);
    assert_eq!(urls, vec!["https://myapp.localhost"]);
}

#[test]
fn test_format_urls_mixed_local_and_external() {
    let domains = vec!["localhost".to_string(), "myapp.com".to_string()];
    let displays = vec![crate::config::DomainDisplay {
        suffix: "myapp.com".to_string(),
        https: true,
        port: None,
    }];
    let urls = format_urls("api", &domains, 1355, false, &displays);
    assert_eq!(
        urls,
        vec!["http://api.localhost:1355", "https://api.myapp.com"]
    );
}

#[test]
fn test_resolve_state_dir() {
    if std::env::var("NSL_STATE_DIR").is_ok() {
        // If env var is set, resolve_state_dir returns that value
        let dir = resolve_state_dir(80);
        assert_eq!(dir, PathBuf::from(std::env::var("NSL_STATE_DIR").unwrap()));
    } else {
        let dir = resolve_state_dir(80);
        assert_eq!(dir, crate::platform::privileged_state_dir());

        let dir = resolve_state_dir(1355);
        assert!(dir.to_str().unwrap().ends_with(".nsl"));
    }
}

/// All env-var-dependent sudo detection tests are combined into a single
/// test function to avoid races from parallel test threads mutating the
/// shared process environment.
#[cfg(unix)]
#[test]
fn test_sudo_detection_and_fix_ownership() {
    use crate::platform::unix::detect_sudo_ids;
    // SAFETY: env manipulation in tests. Combined into one test to avoid
    // parallel mutation of the process environment.

    // --- detect_sudo_ids: not set ---
    unsafe {
        std::env::remove_var("SUDO_UID");
        std::env::remove_var("SUDO_GID");
    }
    assert!(
        detect_sudo_ids().is_none(),
        "should be None when env vars not set"
    );

    // --- fix_ownership: no-op without sudo ---
    fix_ownership(Path::new("/tmp/nonexistent_test_file"));

    // --- detect_sudo_ids: both set ---
    unsafe {
        std::env::set_var("SUDO_UID", "1000");
        std::env::set_var("SUDO_GID", "1000");
    }
    assert_eq!(
        detect_sudo_ids(),
        Some((1000, 1000)),
        "should parse valid uid/gid"
    );

    // --- fix_ownership: graceful on nonexistent path ---
    fix_ownership(Path::new("/tmp/nsl_test_nonexistent_path_abc123"));

    // --- detect_sudo_ids: only uid set ---
    unsafe {
        std::env::remove_var("SUDO_GID");
    }
    assert!(
        detect_sudo_ids().is_none(),
        "should be None when SUDO_GID missing"
    );

    // --- detect_sudo_ids: invalid values ---
    unsafe {
        std::env::set_var("SUDO_UID", "not_a_number");
        std::env::set_var("SUDO_GID", "1000");
    }
    assert!(
        detect_sudo_ids().is_none(),
        "should be None for non-numeric uid"
    );

    // --- cleanup ---
    unsafe {
        std::env::remove_var("SUDO_UID");
        std::env::remove_var("SUDO_GID");
    }
}

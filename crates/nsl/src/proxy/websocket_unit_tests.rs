use super::*;

/// Standalone version of is_upgrade_request that works on a HeaderMap.
fn is_upgrade_headers(headers: &hyper::HeaderMap) -> bool {
    let has_upgrade_connection = headers
        .get(hyper::header::CONNECTION)
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.split(',')
                .any(|token| token.trim().eq_ignore_ascii_case("upgrade"))
        })
        .unwrap_or(false);

    let has_websocket_upgrade = headers
        .get(hyper::header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("websocket"))
        .unwrap_or(false);

    has_upgrade_connection && has_websocket_upgrade
}

#[test]
fn test_is_upgrade_request_valid_websocket() {
    let mut headers = hyper::HeaderMap::new();
    headers.insert(hyper::header::CONNECTION, "Upgrade".parse().unwrap());
    headers.insert(hyper::header::UPGRADE, "websocket".parse().unwrap());
    assert!(is_upgrade_headers(&headers));
}

#[test]
fn test_is_upgrade_request_case_insensitive() {
    let mut headers = hyper::HeaderMap::new();
    headers.insert(hyper::header::CONNECTION, "upgrade".parse().unwrap());
    headers.insert(hyper::header::UPGRADE, "WebSocket".parse().unwrap());
    assert!(is_upgrade_headers(&headers));
}

#[test]
fn test_is_upgrade_request_connection_with_multiple_tokens() {
    let mut headers = hyper::HeaderMap::new();
    headers.insert(
        hyper::header::CONNECTION,
        "keep-alive, Upgrade".parse().unwrap(),
    );
    headers.insert(hyper::header::UPGRADE, "websocket".parse().unwrap());
    assert!(is_upgrade_headers(&headers));
}

#[test]
fn test_is_upgrade_request_missing_upgrade_header() {
    let mut headers = hyper::HeaderMap::new();
    headers.insert(hyper::header::CONNECTION, "Upgrade".parse().unwrap());
    assert!(!is_upgrade_headers(&headers));
}

#[test]
fn test_is_upgrade_request_missing_connection_header() {
    let mut headers = hyper::HeaderMap::new();
    headers.insert(hyper::header::UPGRADE, "websocket".parse().unwrap());
    assert!(!is_upgrade_headers(&headers));
}

#[test]
fn test_is_upgrade_request_non_websocket_upgrade() {
    let mut headers = hyper::HeaderMap::new();
    headers.insert(hyper::header::CONNECTION, "Upgrade".parse().unwrap());
    headers.insert(hyper::header::UPGRADE, "h2c".parse().unwrap());
    assert!(!is_upgrade_headers(&headers));
}

#[test]
fn test_is_upgrade_request_no_headers() {
    let headers = hyper::HeaderMap::new();
    assert!(!is_upgrade_headers(&headers));
}

#[test]
fn test_parse_upstream_response_101() {
    let raw = b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: abc123\r\n\r\n";
    let result = parse_upstream_response(raw);
    assert!(result.is_some());
    let (status, end) = result.unwrap();
    assert_eq!(status, 101);
    assert_eq!(end, raw.len());
}

#[test]
fn test_parse_upstream_response_403() {
    let raw = b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n";
    let result = parse_upstream_response(raw);
    assert!(result.is_some());
    let (status, end) = result.unwrap();
    assert_eq!(status, 403);
    assert_eq!(end, raw.len());
}

#[test]
fn test_parse_upstream_response_incomplete() {
    let raw = b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n";
    let result = parse_upstream_response(raw);
    assert!(result.is_none());
}

#[test]
fn test_parse_upstream_response_with_extra_body() {
    let raw = b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\r\nextra body data";
    let result = parse_upstream_response(raw);
    assert!(result.is_some());
    let (status, end) = result.unwrap();
    assert_eq!(status, 101);
    assert_eq!(&raw[end..], b"extra body data");
}

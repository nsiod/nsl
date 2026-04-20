use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::routes::RouteMapping;

use super::{NSL_HEADER, NSL_HOPS_HEADER, ProxyBody, response, strip_path_prefix};

// ---------------------------------------------------------------------------
// Upgrade detection
// ---------------------------------------------------------------------------

/// Check if a request is a WebSocket upgrade request.
///
/// A valid WebSocket upgrade has both `Connection: Upgrade` (case-insensitive
/// token list) and `Upgrade: websocket` (case-insensitive value).
pub(super) fn is_upgrade_request(req: &Request<Incoming>) -> bool {
    let has_upgrade_connection = req
        .headers()
        .get(hyper::header::CONNECTION)
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.split(',')
                .any(|token| token.trim().eq_ignore_ascii_case("upgrade"))
        })
        .unwrap_or(false);

    let has_websocket_upgrade = req
        .headers()
        .get(hyper::header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("websocket"))
        .unwrap_or(false);

    has_upgrade_connection && has_websocket_upgrade
}

// ---------------------------------------------------------------------------
// Raw upgrade request builder
// ---------------------------------------------------------------------------

/// Build the raw HTTP request bytes to send to the upstream server for a
/// WebSocket upgrade.
fn build_raw_upgrade_request(
    req: &Request<Incoming>,
    host: &str,
    proxy_port: u16,
    hops: u32,
    route: &RouteMapping,
    path_and_query: &str,
    is_tls: bool,
) -> Vec<u8> {
    let method = req.method().as_str();
    let mut raw = format!("{} {} HTTP/1.1\r\n", method, path_and_query);

    for (key, value) in req.headers() {
        let key_str = key.as_str();
        if key_str.starts_with(':') {
            continue;
        }
        if route.change_origin && key == hyper::header::HOST {
            continue;
        }
        if let Ok(v) = value.to_str() {
            // Sanitize header value to prevent CRLF injection.
            let sanitized = v.replace(['\r', '\n'], "");
            raw.push_str(&format!("{}: {}\r\n", key_str, sanitized));
        }
    }

    if route.change_origin {
        raw.push_str(&format!("host: 127.0.0.1:{}\r\n", route.port));
    }

    if !req.headers().contains_key("x-forwarded-for") {
        raw.push_str("x-forwarded-for: 127.0.0.1\r\n");
    }
    if !req.headers().contains_key("x-forwarded-proto") {
        raw.push_str(if is_tls {
            "x-forwarded-proto: https\r\n"
        } else {
            "x-forwarded-proto: http\r\n"
        });
    }
    if !req.headers().contains_key("x-forwarded-host") {
        raw.push_str(&format!("x-forwarded-host: {}\r\n", host));
    }
    if !req.headers().contains_key("x-forwarded-port") {
        raw.push_str(&format!("x-forwarded-port: {}\r\n", proxy_port));
    }

    raw.push_str(&format!("{}: {}\r\n", NSL_HOPS_HEADER, hops + 1));
    raw.push_str(&format!("{}: 1\r\n", NSL_HEADER));
    raw.push_str("\r\n");
    raw.into_bytes()
}

/// Parse the upstream HTTP response to find the status code and the offset
/// where headers end. Returns `Some((status_code, header_end_offset))` when
/// the full header block has been received.
fn parse_upstream_response(buf: &[u8]) -> Option<(u16, usize)> {
    let header_end = buf.windows(4).position(|w| w == b"\r\n\r\n")?;
    let end_offset = header_end + 4;

    let status_line_end = buf.windows(2).position(|w| w == b"\r\n")?;
    let status_line = std::str::from_utf8(&buf[..status_line_end]).ok()?;
    let parts: Vec<&str> = status_line.splitn(3, ' ').collect();
    if parts.len() < 2 {
        return None;
    }
    let status_code: u16 = parts[1].parse().ok()?;
    Some((status_code, end_offset))
}

// ---------------------------------------------------------------------------
// WebSocket upgrade handler
// ---------------------------------------------------------------------------

/// Handle a WebSocket upgrade request by connecting to the upstream server,
/// forwarding the upgrade handshake, and piping data bidirectionally.
pub(super) async fn handle_upgrade(
    req: Request<Incoming>,
    proxy_port: u16,
    host: String,
    hops: u32,
    route: &RouteMapping,
    is_tls: bool,
) -> Result<Response<ProxyBody>, hyper::Error> {
    let target_port = route.port;

    let req_path = req.uri().path().to_string();
    let forwarded_path = if route.strip_prefix && route.path_prefix != "/" {
        strip_path_prefix(&req_path, &route.path_prefix)
    } else {
        req_path.clone()
    };
    let query = req.uri().query();
    let path_and_query = match query {
        Some(q) => format!("{}?{}", forwarded_path, q),
        None => forwarded_path,
    };

    let raw_request = build_raw_upgrade_request(
        &req,
        &host,
        proxy_port,
        hops,
        route,
        &path_and_query,
        is_tls,
    );

    // Connect to upstream
    let upstream = match TcpStream::connect(format!("127.0.0.1:{}", target_port)).await {
        Ok(s) => s,
        Err(_) => {
            return Ok(response(
                StatusCode::BAD_GATEWAY,
                "The target app is not responding. It may have crashed.",
            ));
        }
    };

    let (mut upstream_read, mut upstream_write) = tokio::io::split(upstream);

    // Send the raw upgrade request to upstream
    if let Err(e) = upstream_write.write_all(&raw_request).await {
        tracing::error!("failed to send upgrade request to upstream: {}", e);
        return Ok(response(
            StatusCode::BAD_GATEWAY,
            "Failed to send upgrade request to upstream.",
        ));
    }

    // Read the upstream response headers
    let mut resp_buf = Vec::with_capacity(4096);
    let mut tmp = [0u8; 4096];
    let (status_code, header_end) = loop {
        let n = match upstream_read.read(&mut tmp).await {
            Ok(0) => {
                return Ok(response(
                    StatusCode::BAD_GATEWAY,
                    "Upstream closed connection before completing upgrade handshake.",
                ));
            }
            Ok(n) => n,
            Err(e) => {
                tracing::error!("error reading upstream upgrade response: {}", e);
                return Ok(response(
                    StatusCode::BAD_GATEWAY,
                    "Error reading upstream upgrade response.",
                ));
            }
        };
        resp_buf.extend_from_slice(&tmp[..n]);

        if let Some(parsed) = parse_upstream_response(&resp_buf) {
            break parsed;
        }

        if resp_buf.len() > 65536 {
            return Ok(response(
                StatusCode::BAD_GATEWAY,
                "Upstream response headers too large.",
            ));
        }
    };

    if status_code != 101 {
        // Backend rejected the upgrade -- forward the response as normal HTTP
        tracing::debug!(
            "upstream rejected WebSocket upgrade with status {}",
            status_code
        );

        let head_bytes = &resp_buf[..header_end];
        let head_str = String::from_utf8_lossy(head_bytes);
        let mut builder = Response::builder()
            .status(StatusCode::from_u16(status_code).unwrap_or(StatusCode::BAD_GATEWAY));

        for line in head_str.lines().skip(1) {
            let line = line.trim_end_matches('\r');
            if line.is_empty() {
                break;
            }
            if let Some((k, v)) = line.split_once(':') {
                builder = builder.header(k.trim(), v.trim());
            }
        }
        builder = builder.header(NSL_HEADER, "1");

        // Collect any body bytes already received plus a short drain
        let mut body_data = resp_buf[header_end..].to_vec();
        let mut extra = [0u8; 8192];
        loop {
            match tokio::time::timeout(
                std::time::Duration::from_millis(100),
                upstream_read.read(&mut extra),
            )
            .await
            {
                Ok(Ok(0)) | Err(_) => break,
                Ok(Ok(n)) => body_data.extend_from_slice(&extra[..n]),
                Ok(Err(_)) => break,
            }
        }

        return match builder.body(
            Full::new(Bytes::from(body_data))
                .map_err(|never| match never {})
                .boxed(),
        ) {
            Ok(resp) => Ok(resp),
            Err(e) => {
                tracing::warn!("failed to build forwarded response: {e}");
                Ok(response(
                    StatusCode::BAD_GATEWAY,
                    "Invalid upstream response headers.",
                ))
            }
        };
    }

    // Status 101 -- upgrade succeeded
    let raw_head = resp_buf[..header_end].to_vec();
    let extra_data = resp_buf[header_end..].to_vec();

    // Build the 101 response to return to hyper so it releases the connection
    let resp_for_hyper = {
        let head_str = String::from_utf8_lossy(&raw_head);
        let mut builder = Response::builder().status(StatusCode::SWITCHING_PROTOCOLS);

        for line in head_str.lines().skip(1) {
            let line = line.trim_end_matches('\r');
            if line.is_empty() {
                break;
            }
            if let Some((k, v)) = line.split_once(':') {
                builder = builder.header(k.trim(), v.trim());
            }
        }

        match builder.body(
            Full::new(Bytes::new())
                .map_err(|never| match never {})
                .boxed(),
        ) {
            Ok(resp) => resp,
            Err(e) => {
                tracing::warn!("failed to build upgrade response: {e}");
                return Ok(response(
                    StatusCode::BAD_GATEWAY,
                    "Invalid upstream upgrade headers.",
                ));
            }
        }
    };

    // Spawn the bidirectional pipe in a background task
    tokio::task::spawn(async move {
        let upgraded = match hyper::upgrade::on(req).await {
            Ok(u) => u,
            Err(e) => {
                tracing::error!("client upgrade failed: {}", e);
                return;
            }
        };

        let mut client_io = TokioIo::new(upgraded);
        let mut upstream_stream = upstream_read.unsplit(upstream_write);

        // Forward any data the upstream already sent beyond the headers
        if !extra_data.is_empty()
            && let Err(e) = tokio::io::AsyncWriteExt::write_all(&mut client_io, &extra_data).await
        {
            tracing::debug!("error writing extra data to client: {}", e);
            return;
        }

        match tokio::io::copy_bidirectional(&mut client_io, &mut upstream_stream).await {
            Ok((c2u, u2c)) => {
                tracing::debug!(
                    "WebSocket closed: client->upstream={}, upstream->client={}",
                    c2u,
                    u2c
                );
            }
            Err(e) => {
                tracing::debug!("WebSocket pipe ended: {}", e);
            }
        }
    });

    Ok(resp_for_hyper)
}

#[cfg(test)]
#[path = "websocket_unit_tests.rs"]
mod tests;

//! Public listener: peeks the HTTP Host header (or TLS SNI on the HTTPS
//! variant), looks up the tenant session in the shared
//! [`tunnel::registry::SessionRegistry`], and byte-pumps the TCP stream
//! through a QUIC bi-stream to the matching `nsl` client.
//!
//! Two entry points:
//!
//! * [`run_plain`] — plain HTTP. Used when ACME is disabled or as the
//!   landing port that issues `301 Moved Permanently -> https://` when a
//!   TLS listener is also running.
//! * [`run_https`] — rustls-backed TLS termination. Expects a
//!   [`Arc<dyn rustls::server::ResolvesServerCert>`] that returns a
//!   per-tenant cert based on the ClientHello SNI. After the handshake,
//!   the decrypted stream is routed just like plain HTTP.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;
use tunnel::registry::SessionRegistry;

use crate::forward_auth::{self, AuthDecision, ForwardAuthClient};

const HOST_PEEK_TIMEOUT_SECS: u64 = 10;
const HOST_PEEK_MAX_BYTES: usize = 8 * 1024;
const PEEK_READ_CHUNK: usize = 1024;

/// How the plain-HTTP listener should behave. `Route` = act like a
/// tunnel edge (legacy behavior when ACME is disabled). `Redirect` =
/// issue a `301` redirect to `https://{host}{path}` (used alongside the
/// HTTPS listener so bare-HTTP clients get upgraded).
#[derive(Debug, Clone, Copy)]
pub enum PlainMode {
    Route,
    Redirect,
}

/// Run the plain-HTTP public listener. `forward_auth`, when `Some`,
/// gates every `PlainMode::Route` connection through the external auth
/// endpoint; `Redirect` mode never calls it (the client is being sent
/// straight to HTTPS anyway).
pub async fn run_plain(
    listen: &str,
    registry: SessionRegistry,
    mode: PlainMode,
    forward_auth: Option<Arc<ForwardAuthClient>>,
    cancel: CancellationToken,
) -> Result<()> {
    let addr = parse_listen(listen)?;
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind public HTTP listener on {}", addr))?;
    tracing::info!(%addr, mode = ?mode, forward_auth = forward_auth.is_some(), "public HTTP listener started");

    loop {
        let accept = listener.accept();
        tokio::select! {
            res = accept => match res {
                Ok((stream, remote)) => {
                    let registry = registry.clone();
                    let fwd = forward_auth.clone();
                    let client_ip = remote.ip().to_string();
                    tokio::spawn(async move {
                        let result = match mode {
                            PlainMode::Route => handle_route(stream, registry, fwd, "http", client_ip).await,
                            PlainMode::Redirect => handle_redirect(stream).await,
                        };
                        if let Err(e) = result {
                            tracing::debug!(%remote, error = %e, "public connection ended");
                        }
                    });
                }
                Err(e) => tracing::warn!(error = %e, "public accept error"),
            },
            _ = cancel.cancelled() => {
                tracing::info!("public HTTP listener shutting down");
                return Ok(());
            }
        }
    }
}

/// Run the HTTPS public listener. `cert_resolver` is consulted on every
/// ClientHello; it returns the per-tenant wildcard cert or `None` (which
/// aborts the handshake).
pub async fn run_https(
    listen: &str,
    registry: SessionRegistry,
    cert_resolver: Arc<dyn rustls::server::ResolvesServerCert>,
    forward_auth: Option<Arc<ForwardAuthClient>>,
    cancel: CancellationToken,
) -> Result<()> {
    let addr = parse_listen(listen)?;
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind public HTTPS listener on {}", addr))?;

    let mut config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_cert_resolver(cert_resolver);
    // Mirror the browsers that hit this listener: HTTP/1.1 only for now;
    // HTTP/2 / h3 can come later.
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let acceptor = TlsAcceptor::from(Arc::new(config));
    tracing::info!(%addr, forward_auth = forward_auth.is_some(), "public HTTPS listener started");

    loop {
        let accept = listener.accept();
        tokio::select! {
            res = accept => match res {
                Ok((stream, remote)) => {
                    let acceptor = acceptor.clone();
                    let registry = registry.clone();
                    let fwd = forward_auth.clone();
                    let client_ip = remote.ip().to_string();
                    tokio::spawn(async move {
                        match acceptor.accept(stream).await {
                            Ok(tls) => {
                                if let Err(e) = handle_route(tls, registry, fwd, "https", client_ip).await {
                                    tracing::debug!(%remote, error = %e, "public TLS connection ended");
                                }
                            }
                            Err(e) => tracing::debug!(%remote, error = %e, "TLS handshake failed"),
                        }
                    });
                }
                Err(e) => tracing::warn!(error = %e, "public HTTPS accept error"),
            },
            _ = cancel.cancelled() => {
                tracing::info!("public HTTPS listener shutting down");
                return Ok(());
            }
        }
    }
}

/// Peek the full request header section, (optionally) run forward-auth,
/// look up the session, pump bytes. Works over any duplex stream
/// (plain TCP or TLS-decrypted).
async fn handle_route<S>(
    mut stream: S,
    registry: SessionRegistry,
    forward_auth: Option<Arc<ForwardAuthClient>>,
    scheme: &'static str,
    client_addr: String,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut prefix = Vec::with_capacity(PEEK_READ_CHUNK);
    // With forward-auth on we need the whole header section so we can
    // both parse it and inject auth headers back before the EOH. Even
    // without it we still need the Host, so reading up to EOH (or
    // Host-line-early-exit) is the base behaviour.
    let read_result = tokio::time::timeout(
        Duration::from_secs(HOST_PEEK_TIMEOUT_SECS),
        read_headers_or_host(&mut stream, &mut prefix, forward_auth.is_some()),
    )
    .await;
    let host = match read_result {
        Ok(Ok(h)) => h,
        Ok(Err(e)) => {
            let _ = write_error_response(&mut stream, 400, "Bad Request").await;
            return Err(e);
        }
        Err(_) => {
            let _ = write_error_response(&mut stream, 408, "Request Timeout").await;
            anyhow::bail!("timed out reading request headers");
        }
    };

    // Forward-auth gate. Runs BEFORE the session / QUIC lookup so
    // unauthenticated requests can't even probe which tenant domains
    // are registered.
    let mut injected: Vec<String> = Vec::new();
    if let Some(client) = &forward_auth {
        let parsed = match forward_auth::parse_request(&prefix) {
            Some(p) => p,
            None => {
                let _ = write_error_response(&mut stream, 400, "Bad Request").await;
                anyhow::bail!("forward-auth: could not parse request prefix");
            }
        };
        if !client.cfg().is_bypassed(parsed.path) {
            match client.check(scheme, &client_addr, &parsed).await {
                Ok(AuthDecision::Allow { injected_headers }) => injected = injected_headers,
                Ok(AuthDecision::ShortCircuit { raw_response }) => {
                    let _ = stream.write_all(&raw_response).await;
                    let _ = stream.flush().await;
                    return Ok(());
                }
                Err(e) => {
                    tracing::warn!(error = %e, "forward-auth probe failed; failing closed");
                    let _ =
                        write_error_response(&mut stream, 502, "Bad Gateway").await;
                    return Err(e);
                }
            }
        }
    }

    let session = match registry.find_by_host(&host).await {
        Some(s) => s,
        None => {
            let _ = write_error_response(&mut stream, 502, "Bad Gateway").await;
            anyhow::bail!("no session for host '{}'", host);
        }
    };

    let (mut send, mut recv) = match session.connection.open_bi().await {
        Ok(pair) => pair,
        Err(e) => {
            let _ = write_error_response(&mut stream, 502, "Bad Gateway").await;
            return Err(anyhow::anyhow!("open_bi failed: {}", e));
        }
    };

    // If auth injected headers, rewrite the prefix before forwarding.
    // Without injection the prefix is forwarded verbatim.
    let prefix_to_send = if injected.is_empty() {
        prefix
    } else {
        forward_auth::inject_headers_into_prefix(&prefix, &injected)
    };

    if let Err(e) = send.write_all(&prefix_to_send).await {
        return Err(anyhow::anyhow!("write prefix to tunnel: {}", e));
    }

    let (mut r, mut w) = tokio::io::split(stream);

    let up = async {
        let _ = tokio::io::copy(&mut r, &mut send).await;
        let _ = send.finish();
    };
    let down = async {
        let _ = tokio::io::copy(&mut recv, &mut w).await;
        let _ = w.shutdown().await;
    };

    // Either direction completing closes the other: browsers using
    // `Connection: close` don't send FIN until after they've read the
    // response, so waiting for both halves to finish would deadlock.
    tokio::select! {
        _ = up => {}
        _ = down => {}
    }
    Ok(())
}

/// `PlainMode::Redirect` handler: read request line+Host, respond with a
/// `301 Location: https://{host}{uri}`, close.
async fn handle_redirect(mut stream: TcpStream) -> Result<()> {
    let mut buf = Vec::with_capacity(PEEK_READ_CHUNK);
    tokio::time::timeout(
        Duration::from_secs(HOST_PEEK_TIMEOUT_SECS),
        read_headers(&mut stream, &mut buf),
    )
    .await
    .context("redirect: header read timeout")??;

    let (host, path) =
        parse_request_target(&buf).unwrap_or_else(|| ("".to_string(), "/".to_string()));
    let location = if host.is_empty() {
        "/".to_string()
    } else {
        format!("https://{}{}", host, path)
    };
    let body = format!(
        "<!doctype html><html><body>Moved to <a href=\"{0}\">{0}</a></body></html>",
        location
    );
    let resp = format!(
        "HTTP/1.1 301 Moved Permanently\r\nLocation: {}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        location,
        body.len(),
        body
    );
    stream.write_all(resp.as_bytes()).await?;
    let _ = stream.flush().await;
    Ok(())
}

async fn read_host<S>(stream: &mut S, buf: &mut Vec<u8>) -> Result<String>
where
    S: AsyncRead + Unpin,
{
    let mut chunk = [0u8; PEEK_READ_CHUNK];
    loop {
        if let Some(host) = extract_host(buf) {
            return Ok(host);
        }
        if header_section_ended(buf) {
            anyhow::bail!("request missing Host header");
        }
        if buf.len() >= HOST_PEEK_MAX_BYTES {
            anyhow::bail!("request headers too large");
        }
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            anyhow::bail!("connection closed before Host header");
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// With `need_full_headers = true` (forward-auth path), read until the
/// trailing `\r\n\r\n` so downstream logic can parse + inject. Without
/// it, fall back to the faster `read_host` path that stops as soon as
/// the Host line is available.
async fn read_headers_or_host<S>(
    stream: &mut S,
    buf: &mut Vec<u8>,
    need_full_headers: bool,
) -> Result<String>
where
    S: AsyncRead + Unpin,
{
    if !need_full_headers {
        return read_host(stream, buf).await;
    }
    let mut chunk = [0u8; PEEK_READ_CHUNK];
    loop {
        if header_section_ended(buf) {
            let Some(host) = extract_host(buf) else {
                anyhow::bail!("request missing Host header");
            };
            return Ok(host);
        }
        if buf.len() >= HOST_PEEK_MAX_BYTES {
            anyhow::bail!("request headers too large");
        }
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            anyhow::bail!("connection closed before end of headers");
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

async fn read_headers<S>(stream: &mut S, buf: &mut Vec<u8>) -> Result<()>
where
    S: AsyncRead + Unpin,
{
    let mut chunk = [0u8; PEEK_READ_CHUNK];
    loop {
        if header_section_ended(buf) {
            return Ok(());
        }
        if buf.len() >= HOST_PEEK_MAX_BYTES {
            anyhow::bail!("headers too large");
        }
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

fn extract_host(buf: &[u8]) -> Option<String> {
    let first_crlf = find_subslice(buf, b"\r\n")?;
    let mut pos = first_crlf + 2;
    while pos < buf.len() {
        let line_end = find_subslice(&buf[pos..], b"\r\n")? + pos;
        if line_end == pos {
            return None;
        }
        let line = &buf[pos..line_end];
        if line.len() >= 5 && line[..5].eq_ignore_ascii_case(b"host:") {
            let value = std::str::from_utf8(&line[5..]).ok()?.trim();
            let host = value.split(':').next()?.trim();
            if host.is_empty() {
                return None;
            }
            return Some(host.to_ascii_lowercase());
        }
        pos = line_end + 2;
    }
    None
}

fn parse_request_target(buf: &[u8]) -> Option<(String, String)> {
    let first_crlf = find_subslice(buf, b"\r\n")?;
    let first_line = std::str::from_utf8(&buf[..first_crlf]).ok()?;
    let mut parts = first_line.split_whitespace();
    let _method = parts.next()?;
    let path = parts.next().unwrap_or("/").to_string();
    let host = extract_host(buf).unwrap_or_default();
    Some((host, path))
}

fn header_section_ended(buf: &[u8]) -> bool {
    find_subslice(buf, b"\r\n\r\n").is_some()
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

async fn write_error_response<S>(stream: &mut S, code: u16, reason: &str) -> Result<()>
where
    S: AsyncWrite + Unpin,
{
    let body = format!(
        "<!doctype html><html><body><h1>{} {}</h1></body></html>",
        code, reason
    );
    let resp = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        code,
        reason,
        body.len(),
        body
    );
    stream.write_all(resp.as_bytes()).await?;
    let _ = stream.flush().await;
    Ok(())
}

fn parse_listen(s: &str) -> Result<SocketAddr> {
    if let Some(port) = s.strip_prefix(':') {
        let port: u16 = port
            .parse()
            .with_context(|| format!("invalid listen port: {}", s))?;
        return Ok(SocketAddr::from(([0, 0, 0, 0], port)));
    }
    s.parse::<SocketAddr>()
        .with_context(|| format!("invalid listen addr: {}", s))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_host_with_port() {
        let req = b"GET / HTTP/1.1\r\nHost: foo.example.com:8080\r\nUser-Agent: t\r\n\r\n";
        assert_eq!(extract_host(req), Some("foo.example.com".to_string()));
    }

    #[test]
    fn extracts_host_case_insensitive() {
        let req = b"GET / HTTP/1.1\r\nHOST:   BAR.example.com\r\n\r\n";
        assert_eq!(extract_host(req), Some("bar.example.com".to_string()));
    }

    #[test]
    fn returns_none_when_headers_incomplete() {
        let req = b"GET / HTTP/1.1\r\nHos";
        assert!(extract_host(req).is_none());
    }

    #[test]
    fn parse_request_target_extracts_path_and_host() {
        let req = b"GET /some/path?q=1 HTTP/1.1\r\nHost: foo.example.com\r\n\r\n";
        let (host, path) = parse_request_target(req).unwrap();
        assert_eq!(host, "foo.example.com");
        assert_eq!(path, "/some/path?q=1");
    }

    #[test]
    fn parse_listen_short_form() {
        let a = parse_listen(":8080").unwrap();
        assert_eq!(a.port(), 8080);
    }
}

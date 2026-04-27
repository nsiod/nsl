//! External forward-auth gate for the public edge.
//!
//! When `[forward_auth]` is configured, every public request arriving
//! at `public::run_plain` / `public::run_https` is bounced off an
//! external auth endpoint (Traefik / Caddy / nginx-auth-request style)
//! before the TCP bytes are pumped through the QUIC tunnel to the
//! tenant's nsl client. Only 2xx clears the gate.
//!
//! Wire shape:
//!
//! ```text
//! GET {forward_auth.address}
//!   X-Forwarded-Method:  <original method>
//!   X-Forwarded-Proto:   http | https
//!   X-Forwarded-Host:    <original Host>
//!   X-Forwarded-Uri:     <original path + query>
//!   X-Forwarded-For:     <client IP>
//!   (original Cookie / Authorization are always forwarded)
//! ```
//!
//! Response semantics:
//!
//! - 2xx: allow. Named headers in `response_headers` get appended to
//!   the tenant-bound request before the bytes are pumped over QUIC.
//! - 3xx: short-circuit with the redirect (login flow).
//! - 4xx / 5xx: short-circuit with the status + body.
//!
//! Default: disabled. nsld starts with no auth gate unless the operator
//! explicitly turns it on.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use bytes::Bytes;

const DEFAULT_TIMEOUT_SECS: u64 = 5;

#[derive(Debug, Clone)]
pub struct ForwardAuthConfig {
    pub address: String,
    pub response_headers: Vec<String>,
    pub request_headers: Vec<String>,
    pub timeout: Duration,
    pub bypass_prefixes: Vec<String>,
    pub tls_verify: bool,
}

impl ForwardAuthConfig {
    pub fn new(
        address: String,
        response_headers: Vec<String>,
        request_headers: Vec<String>,
        timeout_secs: Option<u64>,
        bypass_prefixes: Vec<String>,
        tls_verify: bool,
    ) -> Result<Self> {
        if address.trim().is_empty() {
            return Err(anyhow!("forward_auth.address is required"));
        }
        Ok(Self {
            address,
            response_headers,
            request_headers,
            timeout: Duration::from_secs(timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS)),
            bypass_prefixes,
            tls_verify,
        })
    }

    pub fn is_bypassed(&self, path: &str) -> bool {
        self.bypass_prefixes
            .iter()
            .any(|p| !p.is_empty() && path.starts_with(p))
    }
}

/// Headers parsed out of a raw HTTP/1.x request prefix. Only what the
/// gate needs — method, path, host, and the set of (name, value) pairs
/// we may forward to the auth endpoint.
#[derive(Debug, Default)]
pub struct ParsedRequest<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub host: &'a str,
    /// `(lowercase_name, value)` pairs from the raw bytes.
    pub headers: Vec<(String, &'a str)>,
}

/// Parse just enough of an HTTP/1.x request prefix to drive the gate.
/// Expects `buf` to contain bytes up through the trailing `\r\n\r\n`.
pub fn parse_request(buf: &[u8]) -> Option<ParsedRequest<'_>> {
    let end = find_subslice(buf, b"\r\n\r\n")?;
    let head = std::str::from_utf8(&buf[..end]).ok()?;
    let mut lines = head.split("\r\n");
    let first = lines.next()?;
    let mut parts = first.splitn(3, ' ');
    let method = parts.next()?;
    let path = parts.next().unwrap_or("/");
    let mut headers = Vec::new();
    let mut host = "";
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            let lname = name.trim().to_ascii_lowercase();
            let v = value.trim();
            if lname == "host" && host.is_empty() {
                host = v.split(':').next().unwrap_or(v);
            }
            headers.push((lname, v));
        }
    }
    Some(ParsedRequest {
        method,
        path,
        host,
        headers,
    })
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Result of a gate probe.
#[derive(Debug)]
pub enum AuthDecision {
    /// Request is allowed. `injected_headers` is a list of
    /// `"Name: value\r\n"` lines to insert before the trailing
    /// `\r\n\r\n` of the request prefix.
    Allow { injected_headers: Vec<String> },
    /// Auth rejected or redirected. Caller writes this raw HTTP/1.1
    /// response verbatim to the client and closes.
    ShortCircuit { raw_response: Bytes },
}

#[derive(Clone)]
pub struct ForwardAuthClient {
    cfg: Arc<ForwardAuthConfig>,
    http: reqwest::Client,
    forwarded_request_headers: Arc<HashSet<String>>,
}

impl ForwardAuthClient {
    pub fn new(cfg: ForwardAuthConfig) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(cfg.timeout)
            .danger_accept_invalid_certs(!cfg.tls_verify)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("building forward-auth HTTP client")?;
        let mut set: HashSet<String> = cfg
            .request_headers
            .iter()
            .map(|s| s.to_ascii_lowercase())
            .collect();
        // Always-forwarded credentials headers.
        set.insert("cookie".into());
        set.insert("authorization".into());
        Ok(Self {
            cfg: Arc::new(cfg),
            http,
            forwarded_request_headers: Arc::new(set),
        })
    }

    pub fn cfg(&self) -> &ForwardAuthConfig {
        &self.cfg
    }

    pub async fn check(
        &self,
        scheme: &str,
        client_addr: &str,
        parsed: &ParsedRequest<'_>,
    ) -> Result<AuthDecision> {
        let mut req = self
            .http
            .get(&self.cfg.address)
            .header("X-Forwarded-Method", parsed.method)
            .header("X-Forwarded-Proto", scheme)
            .header("X-Forwarded-Host", parsed.host)
            .header("X-Forwarded-Uri", parsed.path);
        if !client_addr.is_empty() {
            req = req.header("X-Forwarded-For", client_addr);
        }
        for (lname, value) in &parsed.headers {
            if self.forwarded_request_headers.contains(lname) {
                req = req.header(lname.as_str(), *value);
            }
        }

        let resp = req
            .send()
            .await
            .with_context(|| format!("forward-auth probe to {}", self.cfg.address))?;
        let status = resp.status();

        if status.is_success() {
            let mut injected = Vec::new();
            for name in &self.cfg.response_headers {
                if let Some(val) = resp.headers().get(name)
                    && let Ok(v) = val.to_str()
                {
                    injected.push(format!("{}: {}\r\n", name, v));
                }
            }
            return Ok(AuthDecision::Allow {
                injected_headers: injected,
            });
        }

        // Non-2xx → serialize the response into an HTTP/1.1 reply for
        // the public client. We read the auth body before copying
        // headers so `Content-Length` lines up with what we actually
        // send.
        let status_u16 = status.as_u16();
        let reason = status.canonical_reason().unwrap_or("");
        let mut header_lines: Vec<String> = Vec::new();
        let mut saw_content_length = false;
        for (name, value) in resp.headers().iter() {
            let lname = name.as_str().to_ascii_lowercase();
            if matches!(
                lname.as_str(),
                "connection" | "keep-alive" | "te" | "trailer" | "transfer-encoding" | "upgrade"
            ) {
                continue;
            }
            let Ok(v) = value.to_str() else {
                continue;
            };
            if lname == "content-length" {
                saw_content_length = true;
            }
            header_lines.push(format!("{}: {}\r\n", name.as_str(), v));
        }
        let body = resp.bytes().await.unwrap_or_default();
        let mut out = format!("HTTP/1.1 {} {}\r\n", status_u16, reason);
        for line in &header_lines {
            out.push_str(line);
        }
        if !saw_content_length {
            out.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        out.push_str("Connection: close\r\n\r\n");
        let mut raw = out.into_bytes();
        raw.extend_from_slice(&body);
        Ok(AuthDecision::ShortCircuit {
            raw_response: Bytes::from(raw),
        })
    }
}

/// Rewrite a buffered request prefix so that `injected` header lines
/// appear right before the terminating `\r\n\r\n`. `prefix` is assumed
/// to contain the full header section (ends with `\r\n\r\n`); if not
/// present, the buffer is returned unchanged (safer than silently
/// corrupting the stream).
pub fn inject_headers_into_prefix(prefix: &[u8], injected: &[String]) -> Vec<u8> {
    if injected.is_empty() {
        return prefix.to_vec();
    }
    let Some(eoh) = find_subslice(prefix, b"\r\n\r\n") else {
        return prefix.to_vec();
    };
    let injected_bytes: String = injected.concat();
    let mut out = Vec::with_capacity(prefix.len() + injected_bytes.len());
    out.extend_from_slice(&prefix[..eoh]);
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(injected_bytes.as_bytes());
    // Drop the leading `\r\n` we just re-inserted above and paste the
    // original terminator back in place.
    // Easier: after `\r\n` we need the final `\r\n` to close headers.
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(&prefix[eoh + 4..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn stub(
        listener: TcpListener,
        status: &'static str,
        extra_headers: &'static [&'static str],
        body: &'static str,
        captured: Arc<Mutex<Vec<u8>>>,
    ) {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 16 * 1024];
        let n = stream.read(&mut buf).await.unwrap();
        captured.lock().unwrap().extend_from_slice(&buf[..n]);
        let mut resp = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\n", body.len());
        for h in extra_headers {
            resp.push_str(h);
            resp.push_str("\r\n");
        }
        resp.push_str("Connection: close\r\n\r\n");
        resp.push_str(body);
        stream.write_all(resp.as_bytes()).await.unwrap();
        stream.flush().await.unwrap();
    }

    fn make_parsed<'a>(raw: &'a str) -> ParsedRequest<'a> {
        parse_request(raw.as_bytes()).unwrap()
    }

    #[tokio::test]
    async fn allow_returns_named_injected_headers() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let cap = Arc::new(Mutex::new(Vec::new()));
        let cap2 = cap.clone();
        let task = tokio::spawn(async move {
            stub(
                listener,
                "200 OK",
                &["X-Auth-User: alice", "X-Irrelevant: nope"],
                "",
                cap2,
            )
            .await
        });

        let cfg = ForwardAuthConfig::new(
            format!("http://127.0.0.1:{}/verify", port),
            vec!["X-Auth-User".into()],
            vec![],
            Some(2),
            vec![],
            true,
        )
        .unwrap();
        let client = ForwardAuthClient::new(cfg).unwrap();

        let raw = "GET /dashboard HTTP/1.1\r\nHost: app.example.com\r\nCookie: s=abc\r\nX-Custom: ignored\r\n\r\n";
        let parsed = make_parsed(raw);

        let decision = client.check("https", "1.2.3.4", &parsed).await.unwrap();
        task.await.unwrap();

        match decision {
            AuthDecision::Allow { injected_headers } => {
                assert_eq!(injected_headers.len(), 1);
                assert_eq!(injected_headers[0], "X-Auth-User: alice\r\n");
            }
            _ => panic!("expected Allow"),
        }

        let text = String::from_utf8_lossy(&cap.lock().unwrap()).to_lowercase();
        assert!(text.contains("x-forwarded-method: get"));
        assert!(text.contains("x-forwarded-host: app.example.com"));
        assert!(text.contains("x-forwarded-uri: /dashboard"));
        assert!(text.contains("cookie: s=abc"));
        assert!(!text.contains("x-custom"));
    }

    #[tokio::test]
    async fn short_circuits_on_401() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let cap = Arc::new(Mutex::new(Vec::new()));
        let cap2 = cap.clone();
        let task = tokio::spawn(async move {
            stub(
                listener,
                "401 Unauthorized",
                &["WWW-Authenticate: Basic realm=\"test\""],
                r#"{"error":"nope"}"#,
                cap2,
            )
            .await
        });

        let cfg = ForwardAuthConfig::new(
            format!("http://127.0.0.1:{}/verify", port),
            vec![],
            vec![],
            Some(2),
            vec![],
            true,
        )
        .unwrap();
        let client = ForwardAuthClient::new(cfg).unwrap();

        let raw = "GET /secret HTTP/1.1\r\nHost: app.example.com\r\n\r\n";
        let parsed = make_parsed(raw);
        let decision = client.check("https", "", &parsed).await.unwrap();
        task.await.unwrap();

        match decision {
            AuthDecision::ShortCircuit { raw_response } => {
                let text = String::from_utf8_lossy(&raw_response);
                assert!(text.starts_with("HTTP/1.1 401 Unauthorized\r\n"));
                assert!(text.contains("www-authenticate: Basic realm=\"test\""));
                assert!(text.contains(r#"{"error":"nope"}"#));
            }
            _ => panic!("expected ShortCircuit"),
        }
    }

    #[test]
    fn inject_headers_places_them_before_eoh() {
        let prefix = b"GET / HTTP/1.1\r\nHost: x\r\n\r\nBODY";
        let injected = vec!["X-Auth-User: alice\r\n".to_string()];
        let out = inject_headers_into_prefix(prefix, &injected);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("Host: x\r\n"));
        assert!(text.contains("X-Auth-User: alice\r\n"));
        assert!(text.ends_with("\r\n\r\nBODY"));
    }

    #[test]
    fn parse_request_extracts_method_path_host() {
        let raw = "POST /api/foo?x=1 HTTP/1.1\r\nHost: app.example.com:443\r\nCookie: s=1\r\n\r\n";
        let p = parse_request(raw.as_bytes()).unwrap();
        assert_eq!(p.method, "POST");
        assert_eq!(p.path, "/api/foo?x=1");
        assert_eq!(p.host, "app.example.com");
        assert!(p.headers.iter().any(|(n, v)| n == "cookie" && *v == "s=1"));
    }

    #[test]
    fn bypass_prefix_is_bypassed() {
        let cfg = ForwardAuthConfig::new(
            "http://a/".into(),
            vec![],
            vec![],
            None,
            vec!["/_nsl/".into(), "/health".into()],
            true,
        )
        .unwrap();
        assert!(cfg.is_bypassed("/_nsl/x"));
        assert!(cfg.is_bypassed("/health"));
        assert!(!cfg.is_bypassed("/api/secret"));
    }
}

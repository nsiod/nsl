//! DNS-01 challenge webhook client (lego `httpreq` convention).
//!
//! Protocol:
//!
//! ```text
//! POST {url}/present    { "fqdn": "_acme-challenge.X.", "value": "<base64>" }
//! POST {url}/cleanup    { "fqdn": "_acme-challenge.X.", "value": "<base64>" }
//! ```
//!
//! Authentication: HTTP Basic Auth (`Authorization: Basic
//! base64(user:pass)`). This matches the lego project's `HTTPREQ_USERNAME`
//! / `HTTPREQ_PASSWORD` environment variables and services that follow
//! the same convention (e.g. dnsall.com).

use anyhow::{Context, Result};
use serde::Serialize;

#[derive(Debug, Clone)]
pub struct HttpreqClient {
    base_url: String,
    basic_auth: Option<(String, String)>,
    http: reqwest::Client,
}

#[derive(Debug, Serialize)]
struct Payload<'a> {
    fqdn: &'a str,
    value: &'a str,
}

impl HttpreqClient {
    /// Build a new client. `username` and `password` are both `Option`;
    /// when either is empty or `None` we skip Basic Auth entirely.
    pub fn new(base_url: &str, username: Option<String>, password: Option<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .context("building reqwest client")?;
        let basic_auth = match (username, password) {
            (Some(u), Some(p)) if !u.trim().is_empty() => Some((u, p)),
            _ => None,
        };
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            basic_auth,
            http,
        })
    }

    pub async fn present(&self, fqdn: &str, value: &str) -> Result<()> {
        self.post("present", fqdn, value).await
    }

    pub async fn cleanup(&self, fqdn: &str, value: &str) -> Result<()> {
        self.post("cleanup", fqdn, value).await
    }

    async fn post(&self, action: &str, fqdn: &str, value: &str) -> Result<()> {
        let url = format!("{}/{}", self.base_url, action);
        let mut req = self.http.post(&url).json(&Payload { fqdn, value });
        if let Some((user, pass)) = &self.basic_auth {
            req = req.basic_auth(user, Some(pass));
        }
        let resp = req.send().await.with_context(|| format!("POST {}", url))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            // Actionable hint per HTTP status. Kept provider-agnostic so
            // we work with any service that speaks the httpreq
            // convention — no dnsall-specific logic.
            let hint = match status.as_u16() {
                401 => " (hint: check acme.httpreq_username / acme.httpreq_password)",
                403 => {
                    " (hint: the provider likely requires this domain \
                     to be pre-registered out of band — add it in the \
                     provider's dashboard / API, set the matching \
                     CNAME in your DNS, then retry)"
                }
                _ => "",
            };
            anyhow::bail!(
                "httpreq {} for {} returned {}{}: {}",
                action,
                fqdn,
                status,
                hint,
                body.chars().take(400).collect::<String>()
            );
        }
        tracing::info!(action, fqdn, "httpreq ok");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::Mutex;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Minimal HTTP/1.1 server that captures the first request and
    /// answers 204. The listener is bound by the caller and handed in so
    /// that the port is live before the client attempts to connect.
    async fn stub_serve_one(listener: TcpListener, captured: Arc<Mutex<Vec<u8>>>) {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 8192];
        let n = stream.read(&mut buf).await.unwrap();
        captured.lock().unwrap().extend_from_slice(&buf[..n]);
        let resp = b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        stream.write_all(resp).await.unwrap();
        stream.flush().await.unwrap();
    }

    #[tokio::test]
    async fn basic_auth_header_is_emitted() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let captured = Arc::new(Mutex::new(Vec::new()));
        let cap = captured.clone();
        let server_task = tokio::spawn(async move { stub_serve_one(listener, cap).await });

        let client = HttpreqClient::new(
            &format!("http://127.0.0.1:{}", port),
            Some("alice".into()),
            Some("s3cret".into()),
        )
        .unwrap();
        client
            .present("_acme-challenge.alice.example.com.", "xyz")
            .await
            .unwrap();
        server_task.await.unwrap();

        let raw = captured.lock().unwrap().clone();
        let text = String::from_utf8_lossy(&raw).to_string();
        // Header names are normalized by reqwest/hyper to lowercase on
        // the wire, but the base64 value is preserved as-is.
        // base64("alice:s3cret") = "YWxpY2U6czNjcmV0"
        let lower = text.to_ascii_lowercase();
        assert!(
            lower.contains("authorization: basic "),
            "missing basic auth header in:\n{text}"
        );
        assert!(
            text.contains("YWxpY2U6czNjcmV0"),
            "missing base64-encoded credentials in:\n{text}"
        );
        assert!(text.contains("POST /present "));
        assert!(text.contains("_acme-challenge.alice.example.com."));
    }

    /// HTTP/1.1 server that answers with a specific status + body.
    async fn stub_serve_status(
        listener: TcpListener,
        status_line: &'static str,
        body: &'static str,
    ) {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 8192];
        let _ = stream.read(&mut buf).await.unwrap();
        let resp = format!(
            "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(resp.as_bytes()).await.unwrap();
        stream.flush().await.unwrap();
    }

    #[tokio::test]
    async fn forbidden_response_surfaces_registration_hint() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server_task = tokio::spawn(async move {
            stub_serve_status(
                listener,
                "403 Forbidden",
                r#"{"error":"domain_not_authorized"}"#,
            )
            .await
        });

        let client = HttpreqClient::new(
            &format!("http://127.0.0.1:{}", port),
            Some("u".into()),
            Some("p".into()),
        )
        .unwrap();
        let err = client
            .present("_acme-challenge.new.example.com.", "val")
            .await
            .unwrap_err();
        server_task.await.unwrap();

        let msg = format!("{:#}", err);
        assert!(msg.contains("403"), "status missing: {msg}");
        assert!(msg.contains("pre-registered"), "hint missing: {msg}");
        assert!(
            msg.contains("domain_not_authorized"),
            "provider body missing: {msg}"
        );
    }

    #[tokio::test]
    async fn unauthorized_response_surfaces_credentials_hint() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server_task = tokio::spawn(async move {
            stub_serve_status(listener, "401 Unauthorized", r#"{"error":"unauthorized"}"#).await
        });

        let client = HttpreqClient::new(
            &format!("http://127.0.0.1:{}", port),
            Some("u".into()),
            Some("bad".into()),
        )
        .unwrap();
        let err = client
            .cleanup("_acme-challenge.x.example.com.", "val")
            .await
            .unwrap_err();
        server_task.await.unwrap();

        let msg = format!("{:#}", err);
        assert!(msg.contains("401"));
        assert!(msg.contains("httpreq_username"), "hint missing: {msg}");
    }

    #[tokio::test]
    async fn no_auth_header_when_missing() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let captured = Arc::new(Mutex::new(Vec::new()));
        let cap = captured.clone();
        let server_task = tokio::spawn(async move { stub_serve_one(listener, cap).await });

        let client = HttpreqClient::new(&format!("http://127.0.0.1:{}", port), None, None).unwrap();
        client
            .cleanup("_acme-challenge.foo.example.", "v")
            .await
            .unwrap();
        server_task.await.unwrap();

        let text = String::from_utf8_lossy(&captured.lock().unwrap()).to_string();
        assert!(!text.to_lowercase().contains("authorization:"));
        assert!(text.contains("POST /cleanup "));
    }
}

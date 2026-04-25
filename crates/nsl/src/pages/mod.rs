/// Escape HTML special characters to prevent XSS.
pub fn escape_html(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            '"' => output.push_str("&quot;"),
            '\'' => output.push_str("&#x27;"),
            _ => output.push(ch),
        }
    }
    output
}

/// Render a complete branded HTML page.
///
/// - `status`: HTTP status code (e.g. 404)
/// - `status_text`: human-readable status text (e.g. "Not Found")
/// - `body_html`: pre-rendered HTML for the page body (already escaped where needed)
pub fn render_page(status: u16, status_text: &str, body_html: &str) -> String {
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{status} {status_text} - NSL</title>
<style>
:root {{
  --bg: #ffffff;
  --fg: #111111;
  --fg-muted: #666666;
  --border: #e5e5e5;
  --card-bg: #f9f9f9;
  --accent: #0070f3;
  --accent-light: #e6f0ff;
  --code-bg: #f4f4f4;
  --error-bg: #fff0f0;
  --error-fg: #cc0000;
  --radius: 8px;
  --max-width: 640px;
}}
@media (prefers-color-scheme: dark) {{
  :root {{
    --bg: #111111;
    --fg: #ededed;
    --fg-muted: #999999;
    --border: #333333;
    --card-bg: #1a1a1a;
    --accent: #3291ff;
    --accent-light: #1a2744;
    --code-bg: #1e1e1e;
    --error-bg: #2a1010;
    --error-fg: #ff6b6b;
  }}
}}
* {{ margin: 0; padding: 0; box-sizing: border-box; }}
body {{
  font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, Helvetica, Arial, sans-serif;
  background: var(--bg);
  color: var(--fg);
  line-height: 1.6;
  min-height: 100vh;
  display: flex;
  flex-direction: column;
  align-items: center;
  padding: 48px 16px;
}}
.container {{
  width: 100%;
  max-width: var(--max-width);
}}
.header {{
  text-align: center;
  margin-bottom: 32px;
}}
.status-code {{
  font-size: 64px;
  font-weight: 700;
  color: var(--error-fg);
  line-height: 1;
}}
.status-text {{
  font-size: 20px;
  color: var(--fg-muted);
  margin-top: 4px;
}}
.body-content {{
  margin-bottom: 32px;
}}
.card {{
  background: var(--card-bg);
  border: 1px solid var(--border);
  border-radius: var(--radius);
  padding: 16px;
  margin-bottom: 12px;
  transition: border-color 0.15s ease;
}}
.card:hover {{
  border-color: var(--accent);
}}
.card-hostname {{
  font-weight: 600;
  font-size: 15px;
  color: var(--accent);
  word-break: break-all;
}}
.card-detail {{
  font-size: 13px;
  color: var(--fg-muted);
  margin-top: 4px;
}}
.section-title {{
  font-size: 14px;
  font-weight: 600;
  text-transform: uppercase;
  letter-spacing: 0.05em;
  color: var(--fg-muted);
  margin-bottom: 12px;
}}
.tip-box {{
  background: var(--error-bg);
  border: 1px solid var(--border);
  border-radius: var(--radius);
  padding: 16px;
  margin-bottom: 12px;
}}
.tip-box p {{
  margin-bottom: 8px;
}}
.tip-box p:last-child {{
  margin-bottom: 0;
}}
code {{
  font-family: "SF Mono", "Fira Code", "Fira Mono", Menlo, Consolas, monospace;
  font-size: 13px;
  background: var(--code-bg);
  padding: 2px 6px;
  border-radius: 4px;
}}
pre {{
  background: var(--code-bg);
  border: 1px solid var(--border);
  border-radius: var(--radius);
  padding: 12px 16px;
  overflow-x: auto;
  font-size: 13px;
  line-height: 1.5;
}}
pre code {{
  background: none;
  padding: 0;
}}
.footer {{
  text-align: center;
  font-size: 13px;
  color: var(--fg-muted);
  margin-top: auto;
  padding-top: 32px;
}}
.footer a {{
  color: var(--accent);
  text-decoration: none;
}}
.footer a:hover {{
  text-decoration: underline;
}}
</style>
</head>
<body>
<div class="container">
  <div class="header">
    <div class="status-code">{status}</div>
    <div class="status-text">{status_text}</div>
  </div>
  <div class="body-content">
    {body_html}
  </div>
</div>
<div class="footer">
  Powered by <a href="https://github.com/nsiod/nsl">NSL</a>
</div>
</body>
</html>"#,
        status = status,
        status_text = escape_html(status_text),
        body_html = body_html,
    )
}

/// Render the body HTML for a 404 page (unregistered hostname).
pub fn render_not_found_body(hostname: &str) -> String {
    let safe_host = escape_html(hostname);

    format!(
        r#"<p>No application is registered for <code>{host}</code>.</p>
<div style="margin-top:24px">
  <div class="section-title">What to do</div>
  <div class="card">
    <p>Run <code>nsl status</code> to view active routes and proxy configuration.</p>
  </div>
</div>"#,
        host = safe_host,
    )
}

/// Render the body HTML for a 502 page (target app not responding).
pub fn render_bad_gateway_body(hostname: &str) -> String {
    let safe_host = escape_html(hostname);
    format!(
        r#"<div class="tip-box">
  <p>The application registered for <code>{host}</code> is not responding.</p>
  <p>It may have crashed or is still starting up.</p>
</div>
<div style="margin-top:24px">
  <div class="section-title">Troubleshooting</div>
  <div class="card">
    <p>1. Run <code>nsl status</code> to check the route and its port.</p>
  </div>
  <div class="card">
    <p>2. Look at your app's terminal output for errors.</p>
  </div>
  <div class="card">
    <p>3. Try restarting your app:</p>
    <pre><code>nsl run -- your-start-command</code></pre>
  </div>
</div>"#,
        host = safe_host,
    )
}

/// Render the body HTML for a 508 page (proxy loop detected).
pub fn render_loop_detected_body() -> String {
    r#"<div class="tip-box">
  <p>This request has passed through NSL too many times, indicating a proxy loop.</p>
  <p>This usually happens when the target application sends requests back through the proxy.</p>
</div>
<div style="margin-top:24px">
  <div class="section-title">How to Fix</div>
  <div class="card">
    <p>Enable <code>changeOrigin</code> so the backend sees <code>127.0.0.1</code> as the Host header instead of the <code>.localhost</code> hostname:</p>
    <pre><code>nsl route myapp.localhost 3000 --change-origin</code></pre>
    <p style="margin-top:8px">Or when using <code>nsl run</code>:</p>
    <pre><code>nsl run --change-origin -- npm start</code></pre>
  </div>
</div>"#
        .to_string()
}

#[cfg(test)]
mod tests;

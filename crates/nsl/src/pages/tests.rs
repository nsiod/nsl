use super::*;

#[test]
fn test_escape_html_special_chars() {
    assert_eq!(escape_html("&"), "&amp;");
    assert_eq!(escape_html("<"), "&lt;");
    assert_eq!(escape_html(">"), "&gt;");
    assert_eq!(escape_html("\""), "&quot;");
    assert_eq!(escape_html("'"), "&#x27;");
}

#[test]
fn test_escape_html_combined() {
    assert_eq!(
        escape_html("<script>alert(\"xss\")</script>"),
        "&lt;script&gt;alert(&quot;xss&quot;)&lt;/script&gt;"
    );
}

#[test]
fn test_escape_html_passthrough() {
    assert_eq!(escape_html("hello world"), "hello world");
    assert_eq!(escape_html("myapp.localhost"), "myapp.localhost");
    assert_eq!(escape_html(""), "");
}

#[test]
fn test_render_page_contains_structure() {
    let html = render_page(404, "Not Found", "<p>test body</p>");
    assert!(html.contains("<!DOCTYPE html>"));
    assert!(html.contains("<title>404 Not Found - NSL</title>"));
    assert!(html.contains("404"));
    assert!(html.contains("Not Found"));
    assert!(html.contains("<p>test body</p>"));
    assert!(html.contains("NSL"));
}

#[test]
fn test_render_page_has_dark_mode() {
    let html = render_page(502, "Bad Gateway", "");
    assert!(html.contains("prefers-color-scheme: dark"));
}

#[test]
fn test_render_page_has_css_variables() {
    let html = render_page(404, "Not Found", "");
    assert!(html.contains("--bg:"));
    assert!(html.contains("--fg:"));
    assert!(html.contains("--accent:"));
}

#[test]
fn test_render_page_responsive() {
    let html = render_page(404, "Not Found", "");
    assert!(html.contains("viewport"));
    assert!(html.contains("width=device-width"));
}

#[test]
fn test_render_page_escapes_status_text() {
    let html = render_page(404, "<script>xss</script>", "");
    assert!(!html.contains("<script>xss</script>"));
    assert!(html.contains("&lt;script&gt;"));
}

#[test]
fn test_render_not_found_body_mentions_status_command() {
    let body = render_not_found_body("test.localhost");
    assert!(body.contains("test.localhost"));
    assert!(body.contains("nsl status"));
}

#[test]
fn test_render_not_found_body_does_not_leak_routes() {
    let body = render_not_found_body("unknown.localhost");
    assert!(!body.contains("app.localhost"));
    assert!(!body.contains("api.localhost"));
    assert!(!body.contains("port "));
    assert!(!body.contains("changeOrigin"));
}

#[test]
fn test_render_not_found_body_escapes_hostname() {
    let body = render_not_found_body("<script>xss</script>");
    assert!(!body.contains("<script>xss</script>"));
    assert!(body.contains("&lt;script&gt;"));
}

#[test]
fn test_render_bad_gateway_body() {
    let body = render_bad_gateway_body("myapp.localhost");
    assert!(body.contains("myapp.localhost"));
    assert!(body.contains("not responding"));
    assert!(body.contains("Troubleshooting"));
    assert!(body.contains("nsl status"));
}

#[test]
fn test_render_bad_gateway_body_does_not_leak_port() {
    let body = render_bad_gateway_body("myapp.localhost");
    assert!(!body.contains("3000"));
    assert!(!body.contains("port 3000"));
}

#[test]
fn test_render_bad_gateway_body_escapes_hostname() {
    let body = render_bad_gateway_body("<img onerror=alert(1)>");
    assert!(!body.contains("<img onerror"));
    assert!(body.contains("&lt;img onerror"));
}

#[test]
fn test_render_loop_detected_body() {
    let body = render_loop_detected_body();
    assert!(body.contains("proxy loop"));
    assert!(body.contains("changeOrigin"));
    assert!(body.contains("--change-origin"));
}

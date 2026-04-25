use super::handler::handle_request;
use super::{NSL_HEADER, RouteCache};
use crate::routes::RouteStore;
use hyper::service::service_fn;
use hyper::{Request, StatusCode};
use hyper_util::rt::TokioIo;
use hyper_util::server::conn::auto::Builder as AutoBuilder;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::RwLock;

#[tokio::test]
async fn test_http1_routing_with_auto_builder() {
    let backend = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend_port = backend.local_addr().unwrap().port();

    let dir = tempfile::tempdir().unwrap();
    let store = RouteStore::new(dir.path().to_path_buf());
    store
        .add_route("myapp.localhost", backend_port, 0, false, false, "/", false)
        .unwrap();

    let backend_handle = tokio::spawn(async move {
        let (mut stream, _) = backend.accept().await.unwrap();
        let mut buf = vec![0u8; 4096];
        let _ = stream.read(&mut buf).await.unwrap();
        let resp = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK";
        stream.write_all(resp.as_bytes()).await.unwrap();
    });

    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_port = proxy_listener.local_addr().unwrap().port();
    let state_dir = dir.path().to_path_buf();

    let route_cache: RouteCache = Arc::new(RwLock::new(
        RouteStore::new(state_dir).load_routes().unwrap_or_default(),
    ));

    let proxy_handle = tokio::spawn(async move {
        if let Ok((stream, _)) = proxy_listener.accept().await {
            let io = TokioIo::new(stream);
            let cache = Arc::clone(&route_cache);
            let service = service_fn(move |req: Request<hyper::body::Incoming>| {
                let cache = Arc::clone(&cache);
                async move {
                    handle_request(
                        req,
                        proxy_port,
                        10,
                        cache,
                        Arc::new(std::sync::RwLock::new(vec!["localhost".to_string()])),
                        false,
                    )
                    .await
                }
            });
            let _ = AutoBuilder::new(hyper_util::rt::TokioExecutor::new())
                .serve_connection_with_upgrades(io, service)
                .await;
        }
    });

    let mut client_stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", proxy_port))
        .await
        .unwrap();

    let request =
        "GET / HTTP/1.1\r\nHost: myapp.localhost\r\nConnection: close\r\n\r\n".to_string();
    client_stream.write_all(request.as_bytes()).await.unwrap();

    let mut resp_buf = vec![0u8; 4096];
    let n = client_stream.read(&mut resp_buf).await.unwrap();
    let resp_str = String::from_utf8_lossy(&resp_buf[..n]);

    assert!(
        resp_str.contains("200") || resp_str.contains("OK"),
        "expected 200 OK response via auto builder, got: {}",
        resp_str
    );

    drop(client_stream);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(1), proxy_handle).await;
    let _ = tokio::time::timeout(std::time::Duration::from_secs(1), backend_handle).await;
}

#[tokio::test]
async fn test_http2_prior_knowledge_routing() {
    let backend = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend_port = backend.local_addr().unwrap().port();

    let dir = tempfile::tempdir().unwrap();
    let store = RouteStore::new(dir.path().to_path_buf());
    store
        .add_route("myapp.localhost", backend_port, 0, false, false, "/", false)
        .unwrap();

    let backend_handle = tokio::spawn(async move {
        let (mut stream, _) = backend.accept().await.unwrap();
        let mut buf = vec![0u8; 4096];
        let _ = stream.read(&mut buf).await.unwrap();
        let resp = "HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";
        stream.write_all(resp.as_bytes()).await.unwrap();
    });

    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_port = proxy_listener.local_addr().unwrap().port();
    let state_dir = dir.path().to_path_buf();

    let route_cache: RouteCache = Arc::new(RwLock::new(
        RouteStore::new(state_dir).load_routes().unwrap_or_default(),
    ));

    let proxy_handle = tokio::spawn(async move {
        if let Ok((stream, _)) = proxy_listener.accept().await {
            let io = TokioIo::new(stream);
            let cache = Arc::clone(&route_cache);
            let service = service_fn(move |req: Request<hyper::body::Incoming>| {
                let cache = Arc::clone(&cache);
                async move {
                    handle_request(
                        req,
                        proxy_port,
                        10,
                        cache,
                        Arc::new(std::sync::RwLock::new(vec!["localhost".to_string()])),
                        false,
                    )
                    .await
                }
            });
            let _ = AutoBuilder::new(hyper_util::rt::TokioExecutor::new())
                .serve_connection_with_upgrades(io, service)
                .await;
        }
    });

    use http_body_util::Full;
    use hyper::body::Bytes;
    use hyper_util::client::legacy::Client;
    use hyper_util::rt::TokioExecutor;

    let client = Client::builder(TokioExecutor::new())
        .http2_only(true)
        .build_http();

    let uri: hyper::Uri = format!("http://127.0.0.1:{}/", proxy_port).parse().unwrap();

    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .header(hyper::header::HOST, "myapp.localhost")
        .body(Full::new(Bytes::new()))
        .unwrap();

    let resp = client.request(req).await;

    match resp {
        Ok(r) => {
            assert_eq!(
                r.status(),
                StatusCode::OK,
                "HTTP/2 request should be proxied successfully"
            );
            assert!(
                r.headers().contains_key(NSL_HEADER),
                "response should contain X-NSL header"
            );
        }
        Err(e) => {
            panic!("HTTP/2 prior knowledge request failed: {}", e);
        }
    }

    let _ = tokio::time::timeout(std::time::Duration::from_secs(1), proxy_handle).await;
    let _ = tokio::time::timeout(std::time::Duration::from_secs(1), backend_handle).await;
}

#[tokio::test]
async fn test_http2_no_route_returns_404() {
    let dir = tempfile::tempdir().unwrap();

    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_port = proxy_listener.local_addr().unwrap().port();
    let state_dir = dir.path().to_path_buf();

    let route_cache: RouteCache = Arc::new(RwLock::new(
        RouteStore::new(state_dir).load_routes().unwrap_or_default(),
    ));

    let proxy_handle = tokio::spawn(async move {
        if let Ok((stream, _)) = proxy_listener.accept().await {
            let io = TokioIo::new(stream);
            let cache = Arc::clone(&route_cache);
            let service = service_fn(move |req: Request<hyper::body::Incoming>| {
                let cache = Arc::clone(&cache);
                async move {
                    handle_request(
                        req,
                        proxy_port,
                        10,
                        cache,
                        Arc::new(std::sync::RwLock::new(vec!["localhost".to_string()])),
                        false,
                    )
                    .await
                }
            });
            let _ = AutoBuilder::new(hyper_util::rt::TokioExecutor::new())
                .serve_connection_with_upgrades(io, service)
                .await;
        }
    });

    use http_body_util::Full;
    use hyper::body::Bytes;
    use hyper_util::client::legacy::Client;
    use hyper_util::rt::TokioExecutor;

    let client = Client::builder(TokioExecutor::new())
        .http2_only(true)
        .build_http();

    let uri: hyper::Uri = format!("http://127.0.0.1:{}/", proxy_port).parse().unwrap();

    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .header(hyper::header::HOST, "unknown.localhost")
        .body(Full::new(Bytes::new()))
        .unwrap();

    let resp = client.request(req).await;

    match resp {
        Ok(r) => {
            assert_eq!(
                r.status(),
                StatusCode::NOT_FOUND,
                "HTTP/2 request to unknown host should return 404"
            );
        }
        Err(e) => {
            panic!("HTTP/2 request failed: {}", e);
        }
    }

    let _ = tokio::time::timeout(std::time::Duration::from_secs(1), proxy_handle).await;
}

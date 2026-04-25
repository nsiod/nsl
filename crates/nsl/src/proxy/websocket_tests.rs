use super::handler::handle_request;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use hyper_util::server::conn::auto::Builder as AutoBuilder;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::RwLock;

use super::RouteCache;
use crate::routes::RouteStore;

#[tokio::test]
async fn test_websocket_upgrade_end_to_end() {
    let backend = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend_port = backend.local_addr().unwrap().port();

    let dir = tempfile::tempdir().unwrap();
    let store = RouteStore::new(dir.path().to_path_buf());
    store
        .add_route("myapp.localhost", backend_port, 0, false, false, "/", false)
        .unwrap();

    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_port = proxy_listener.local_addr().unwrap().port();
    let state_dir = dir.path().to_path_buf();

    let route_cache: RouteCache = Arc::new(RwLock::new(
        RouteStore::new(state_dir.clone())
            .load_routes()
            .unwrap_or_default(),
    ));

    let proxy_handle = tokio::spawn(async move {
        if let Ok((stream, _)) = proxy_listener.accept().await {
            let io = TokioIo::new(stream);
            let cache = Arc::clone(&route_cache);
            let service = service_fn(move |req: hyper::Request<Incoming>| {
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

    let backend_handle = tokio::spawn(async move {
        let (mut stream, _) = backend.accept().await.unwrap();
        let mut buf = vec![0u8; 4096];
        let n = stream.read(&mut buf).await.unwrap();
        let request_str = String::from_utf8_lossy(&buf[..n]);

        assert!(
            request_str.contains("upgrade: websocket")
                || request_str.contains("Upgrade: websocket"),
            "upgrade request should contain websocket upgrade header"
        );

        let response = "HTTP/1.1 101 Switching Protocols\r\n\
                        Upgrade: websocket\r\n\
                        Connection: Upgrade\r\n\
                        \r\n";
        stream.write_all(response.as_bytes()).await.unwrap();

        let mut echo_buf = [0u8; 1024];
        if let Ok(n) = stream.read(&mut echo_buf).await
            && n > 0
        {
            let _ = stream.write_all(&echo_buf[..n]).await;
        }
    });

    let mut client_stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", proxy_port))
        .await
        .unwrap();

    let upgrade_request = format!(
        "GET / HTTP/1.1\r\n\
         Host: myapp.localhost:{}\r\n\
         Connection: Upgrade\r\n\
         Upgrade: websocket\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
         Sec-WebSocket-Version: 13\r\n\
         \r\n",
        proxy_port
    );
    client_stream
        .write_all(upgrade_request.as_bytes())
        .await
        .unwrap();

    let mut resp_buf = vec![0u8; 4096];
    let n = client_stream.read(&mut resp_buf).await.unwrap();
    let resp_str = String::from_utf8_lossy(&resp_buf[..n]);

    assert!(
        resp_str.contains("101"),
        "proxy should forward 101 response, got: {}",
        resp_str
    );
    assert!(
        resp_str.contains("Upgrade") || resp_str.contains("upgrade"),
        "response should contain Upgrade header"
    );

    let test_data = b"hello websocket";
    client_stream.write_all(test_data).await.unwrap();

    let mut echo_buf = [0u8; 1024];
    match tokio::time::timeout(
        std::time::Duration::from_secs(2),
        client_stream.read(&mut echo_buf),
    )
    .await
    {
        Ok(Ok(n)) if n > 0 => {
            assert_eq!(
                &echo_buf[..n],
                test_data,
                "echoed data should match sent data"
            );
        }
        _ => {}
    }

    drop(client_stream);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(1), proxy_handle).await;
    let _ = tokio::time::timeout(std::time::Duration::from_secs(1), backend_handle).await;
}

#[tokio::test]
async fn test_websocket_upgrade_backend_rejects() {
    let backend = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend_port = backend.local_addr().unwrap().port();

    let dir = tempfile::tempdir().unwrap();
    let store = RouteStore::new(dir.path().to_path_buf());
    store
        .add_route("myapp.localhost", backend_port, 0, false, false, "/", false)
        .unwrap();

    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_port = proxy_listener.local_addr().unwrap().port();
    let state_dir = dir.path().to_path_buf();

    let route_cache: RouteCache = Arc::new(RwLock::new(
        RouteStore::new(state_dir.clone())
            .load_routes()
            .unwrap_or_default(),
    ));

    let proxy_handle = tokio::spawn(async move {
        if let Ok((stream, _)) = proxy_listener.accept().await {
            let io = TokioIo::new(stream);
            let cache = Arc::clone(&route_cache);
            let service = service_fn(move |req: hyper::Request<Incoming>| {
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

    let backend_handle = tokio::spawn(async move {
        let (mut stream, _) = backend.accept().await.unwrap();
        let mut buf = vec![0u8; 4096];
        let _ = stream.read(&mut buf).await.unwrap();

        let response = "HTTP/1.1 403 Forbidden\r\n\
                        Content-Type: text/plain\r\n\
                        Content-Length: 9\r\n\
                        \r\n\
                        Forbidden";
        stream.write_all(response.as_bytes()).await.unwrap();
    });

    let mut client_stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", proxy_port))
        .await
        .unwrap();

    let upgrade_request = format!(
        "GET / HTTP/1.1\r\n\
         Host: myapp.localhost:{}\r\n\
         Connection: Upgrade\r\n\
         Upgrade: websocket\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
         Sec-WebSocket-Version: 13\r\n\
         \r\n",
        proxy_port
    );
    client_stream
        .write_all(upgrade_request.as_bytes())
        .await
        .unwrap();

    let mut resp_buf = vec![0u8; 4096];
    let n = client_stream.read(&mut resp_buf).await.unwrap();
    let resp_str = String::from_utf8_lossy(&resp_buf[..n]);

    assert!(
        resp_str.contains("403"),
        "proxy should forward 403 rejection, got: {}",
        resp_str
    );

    drop(client_stream);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(1), proxy_handle).await;
    let _ = tokio::time::timeout(std::time::Duration::from_secs(1), backend_handle).await;
}

#[tokio::test]
async fn test_websocket_upgrade_loop_detection() {
    let dir = tempfile::tempdir().unwrap();
    let store = RouteStore::new(dir.path().to_path_buf());
    store
        .add_route("myapp.localhost", 9999, 0, false, false, "/", false)
        .unwrap();

    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_port = proxy_listener.local_addr().unwrap().port();
    let state_dir = dir.path().to_path_buf();

    let route_cache: RouteCache = Arc::new(RwLock::new(
        RouteStore::new(state_dir.clone())
            .load_routes()
            .unwrap_or_default(),
    ));

    let proxy_handle = tokio::spawn(async move {
        if let Ok((stream, _)) = proxy_listener.accept().await {
            let io = TokioIo::new(stream);
            let cache = Arc::clone(&route_cache);
            let service = service_fn(move |req: hyper::Request<Incoming>| {
                let cache = Arc::clone(&cache);
                async move {
                    handle_request(
                        req,
                        proxy_port,
                        3,
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

    let upgrade_request = format!(
        "GET / HTTP/1.1\r\n\
         Host: myapp.localhost:{}\r\n\
         Connection: Upgrade\r\n\
         Upgrade: websocket\r\n\
         x-nsl-hops: 3\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
         Sec-WebSocket-Version: 13\r\n\
         \r\n",
        proxy_port
    );
    client_stream
        .write_all(upgrade_request.as_bytes())
        .await
        .unwrap();

    let mut resp_buf = vec![0u8; 4096];
    let n = client_stream.read(&mut resp_buf).await.unwrap();
    let resp_str = String::from_utf8_lossy(&resp_buf[..n]);

    assert!(
        resp_str.contains("508") || resp_str.contains("Loop Detected"),
        "should return loop detected for WebSocket with max hops, got: {}",
        resp_str
    );

    drop(client_stream);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(1), proxy_handle).await;
}

/// Find a free port in the given range.
///
/// Strategy: try a random port first, then scan sequentially.
pub fn find_free_port(range_start: u16, range_end: u16) -> anyhow::Result<u16> {
    use std::net::TcpListener;

    // Try random ports first (up to 10 attempts)
    for _ in 0..10 {
        let port = range_start + (rand_u16() % (range_end - range_start + 1));
        if TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return Ok(port);
        }
    }

    // Sequential fallback
    for port in range_start..=range_end {
        if TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return Ok(port);
        }
    }

    anyhow::bail!("no free port found in range {}-{}", range_start, range_end)
}

/// Simple pseudo-random u16 using time-based seed.
fn rand_u16() -> u16 {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    (t ^ (t >> 16)) as u16
}

/// Inject framework-specific flags (--port, --host) if not already present.
pub fn inject_framework_flags(args: &[String], port: u16) -> Vec<String> {
    let mut result = args.to_vec();

    let has_port = args.iter().any(|a| a.starts_with("--port"));
    let has_host = args.iter().any(|a| a.starts_with("--host"));

    if has_port && has_host {
        return result;
    }

    let cmd_str = args.join(" ");
    let framework = detect_framework(&cmd_str);

    if let Some(fw) = framework {
        if !has_port {
            result.push("--port".to_string());
            result.push(port.to_string());
            if fw.strict_port {
                result.push("--strictPort".to_string());
            }
        }
        if !has_host {
            result.push("--host".to_string());
            result.push(fw.host.to_string());
        }
    }

    result
}

/// Replace the literal app-port placeholder in command arguments.
pub fn replace_port_placeholders(args: &[String], port: u16) -> Vec<String> {
    let port = port.to_string();
    args.iter()
        .map(|arg| arg.replace("NSL_PORT", &port))
        .collect()
}

struct FrameworkHint {
    strict_port: bool,
    host: &'static str,
}

fn detect_framework(cmd: &str) -> Option<FrameworkHint> {
    if cmd.contains("vite") || cmd.contains("react-router") {
        Some(FrameworkHint {
            strict_port: true,
            host: "127.0.0.1",
        })
    } else if cmd.contains("astro") || cmd.contains(" ng ") || cmd.contains("react-native") {
        Some(FrameworkHint {
            strict_port: false,
            host: "127.0.0.1",
        })
    } else if cmd.contains("expo") {
        Some(FrameworkHint {
            strict_port: false,
            host: "localhost",
        })
    } else {
        None
    }
}

/// Wait for an app to become ready by polling a TCP connection.
///
/// Polling strategy: first 5 attempts at 200ms interval, then 500ms.
#[allow(dead_code)]
pub async fn wait_for_app(
    port: u16,
    timeout_secs: u64,
    child: &mut tokio::process::Child,
) -> anyhow::Result<()> {
    if timeout_secs == 0 {
        return Ok(());
    }

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    let mut attempt: u32 = 0;

    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                anyhow::bail!("app exited before becoming ready (exit status: {})", status);
            }
            Err(e) => {
                anyhow::bail!("failed to check child process status: {}", e);
            }
            Ok(None) => {}
        }

        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            return Ok(());
        }

        if tokio::time::Instant::now() >= deadline {
            tracing::warn!(
                "app did not become ready within {}s timeout, continuing anyway",
                timeout_secs
            );
            return Ok(());
        }

        let interval = if attempt < 5 {
            std::time::Duration::from_millis(200)
        } else {
            std::time::Duration::from_millis(500)
        };
        tokio::time::sleep(interval).await;
        attempt += 1;
    }
}

/// Wait for an app to become ready (variant for process-wrap child).
#[cfg(unix)]
pub async fn wait_for_app_wrapped(
    port: u16,
    timeout_secs: u64,
    child: &mut Box<dyn process_wrap::tokio::ChildWrapper>,
) -> anyhow::Result<()> {
    if timeout_secs == 0 {
        return Ok(());
    }

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    let mut attempt: u32 = 0;

    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                anyhow::bail!("app exited before becoming ready (exit status: {})", status);
            }
            Err(e) => {
                anyhow::bail!("failed to check child process status: {}", e);
            }
            Ok(None) => {}
        }

        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            return Ok(());
        }

        if tokio::time::Instant::now() >= deadline {
            tracing::warn!(
                "app did not become ready within {}s timeout, continuing anyway",
                timeout_secs
            );
            return Ok(());
        }

        let interval = if attempt < 5 {
            std::time::Duration::from_millis(200)
        } else {
            std::time::Duration::from_millis(500)
        };
        tokio::time::sleep(interval).await;
        attempt += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_free_port() {
        let port = find_free_port(4000, 4999).unwrap();
        assert!((4000..=4999).contains(&port));

        let listener = std::net::TcpListener::bind(("127.0.0.1", port));
        assert!(listener.is_ok());
    }

    #[test]
    fn test_inject_framework_flags_vite() {
        let args = vec!["npx".to_string(), "vite".to_string()];
        let result = inject_framework_flags(&args, 4000);
        assert!(result.contains(&"--port".to_string()));
        assert!(result.contains(&"4000".to_string()));
        assert!(result.contains(&"--strictPort".to_string()));
        assert!(result.contains(&"--host".to_string()));
        assert!(result.contains(&"127.0.0.1".to_string()));
    }

    #[test]
    fn test_inject_framework_flags_no_override() {
        let args = vec![
            "npx".to_string(),
            "vite".to_string(),
            "--port".to_string(),
            "3000".to_string(),
            "--host".to_string(),
            "0.0.0.0".to_string(),
        ];
        let result = inject_framework_flags(&args, 4000);
        assert_eq!(result, args);
    }

    #[test]
    fn test_inject_framework_flags_unknown() {
        let args = vec!["python".to_string(), "server.py".to_string()];
        let result = inject_framework_flags(&args, 4000);
        assert_eq!(result, args);
    }

    #[test]
    fn test_inject_framework_flags_expo() {
        let args = vec!["npx".to_string(), "expo".to_string(), "start".to_string()];
        let result = inject_framework_flags(&args, 4000);
        assert!(result.contains(&"localhost".to_string()));
    }

    #[test]
    fn test_replace_port_placeholders_whole_arg() {
        let args = vec![
            "./server".to_string(),
            "-port".to_string(),
            "NSL_PORT".to_string(),
        ];

        let result = replace_port_placeholders(&args, 4000);

        assert_eq!(result, vec!["./server", "-port", "4000"]);
    }

    #[test]
    fn test_replace_port_placeholders_inside_arg() {
        let args = vec![
            "./server".to_string(),
            "--addr=127.0.0.1:NSL_PORT".to_string(),
        ];

        let result = replace_port_placeholders(&args, 4000);

        assert_eq!(result, vec!["./server", "--addr=127.0.0.1:4000"]);
    }

    #[tokio::test]
    async fn test_wait_for_app_disabled() {
        let mut child = tokio::process::Command::new("sleep")
            .arg("10")
            .spawn()
            .unwrap();

        let result = wait_for_app(0, 0, &mut child).await;
        assert!(result.is_ok());

        child.kill().await.ok();
    }

    #[tokio::test]
    async fn test_wait_for_app_already_listening() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let mut child = tokio::process::Command::new("sleep")
            .arg("10")
            .spawn()
            .unwrap();

        let result = wait_for_app(port, 5, &mut child).await;
        assert!(result.is_ok());

        child.kill().await.ok();
        drop(listener);
    }

    #[tokio::test]
    async fn test_wait_for_app_child_exits_early() {
        let mut child = tokio::process::Command::new("true").spawn().unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let result = wait_for_app(19999, 5, &mut child).await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("exited before becoming ready"));
    }
}

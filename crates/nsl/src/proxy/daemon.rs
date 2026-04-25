use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use http_body_util::Full;
use hyper::Request;
use hyper::body::Bytes;

use crate::config::Config;

use super::NSL_HEADER;
use super::server::{cleanup_lifecycle_files, start_proxy};

// ---------------------------------------------------------------------------
// Daemon
// ---------------------------------------------------------------------------

const STARTUP_LOCK_DIR: &str = "proxy.start.lock";
const STARTUP_LOCK_MAX_RETRIES: u32 = 50;
const STARTUP_LOCK_RETRY_DELAY: Duration = Duration::from_millis(100);
const STARTUP_LOCK_STALE_THRESHOLD: Duration = Duration::from_secs(15);

struct StartupLock {
    path: PathBuf,
}

impl Drop for StartupLock {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn startup_lock_path(state_dir: &Path) -> PathBuf {
    state_dir.join(STARTUP_LOCK_DIR)
}

fn lock_is_stale(path: &Path, stale_threshold: Duration) -> bool {
    let modified = match fs::metadata(path).and_then(|meta| meta.modified()) {
        Ok(mtime) => mtime,
        Err(_) => return false,
    };

    match SystemTime::now().duration_since(modified) {
        Ok(age) => age > stale_threshold,
        Err(_) => false,
    }
}

async fn acquire_startup_lock_with_params(
    state_dir: &Path,
    max_retries: u32,
    retry_delay: Duration,
    stale_threshold: Duration,
) -> anyhow::Result<StartupLock> {
    fs::create_dir_all(state_dir)?;

    let lock_path = startup_lock_path(state_dir);

    for _ in 0..max_retries {
        match fs::create_dir(&lock_path) {
            Ok(()) => return Ok(StartupLock { path: lock_path }),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if lock_is_stale(&lock_path, stale_threshold) {
                    let _ = fs::remove_dir_all(&lock_path);
                    continue;
                }
                tokio::time::sleep(retry_delay).await;
            }
            Err(e) => return Err(e.into()),
        }
    }

    anyhow::bail!(
        "timed out waiting for proxy startup lock at {}",
        lock_path.display()
    )
}

async fn acquire_startup_lock(state_dir: &Path) -> anyhow::Result<StartupLock> {
    acquire_startup_lock_with_params(
        state_dir,
        STARTUP_LOCK_MAX_RETRIES,
        STARTUP_LOCK_RETRY_DELAY,
        STARTUP_LOCK_STALE_THRESHOLD,
    )
    .await
}

/// Daemonize the current process and start the proxy.
///
/// After `platform::daemonize_self` returns, the caller is the daemonized
/// process (Unix: post-fork child; Windows: the already-detached process).
pub fn daemonize_and_start_proxy(config: &Config) -> anyhow::Result<()> {
    let state_dir = config.resolve_state_dir();
    fs::create_dir_all(&state_dir)?;

    crate::platform::daemonize_self(&state_dir)?;

    let log_writer = fs::File::options()
        .append(true)
        .open(state_dir.join("proxy.log"))
        .or_else(|_| fs::File::create(state_dir.join("proxy.log")))?;
    let _ = tracing_subscriber::fmt()
        .with_writer(std::sync::Mutex::new(log_writer))
        .with_ansi(false)
        .with_target(true)
        .with_level(true)
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .try_init();

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(start_proxy(config))?;
    Ok(())
}

fn spawn_proxy_daemon_process(config: &Config) -> anyhow::Result<()> {
    let exe = std::env::current_exe()?;

    let mut args = vec![
        "start".to_string(),
        "--daemonize".to_string(),
        "--listen".to_string(),
        config.proxy_listen(),
    ];
    if config.proxy_https {
        args.push("--https".to_string());
    }

    let mut cmd = std::process::Command::new(exe);
    cmd.args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    // On Windows, apply DETACHED_PROCESS / CREATE_NO_WINDOW so the child
    // survives parent exit and has no inherited console. No-op on Unix
    // (the child will fork+setsid via daemonize_self).
    crate::platform::detach_spawn(&mut cmd);

    cmd.spawn()?;

    Ok(())
}

/// Ensure the proxy is running, serializing concurrent startup attempts.
///
/// Returns `Ok(true)` if this call started the proxy, `Ok(false)` if another
/// process had already started it by the time the lock was acquired.
pub async fn ensure_proxy_running(config: &Config) -> anyhow::Result<bool> {
    let state_dir = config.resolve_state_dir();
    let _lock = acquire_startup_lock(&state_dir).await?;

    if is_proxy_running(config.proxy_port).await {
        return Ok(false);
    }

    spawn_proxy_daemon_process(config)?;

    let log_path = state_dir.join("proxy.log");
    if wait_for_proxy(config.proxy_port, 20, 250).await {
        Ok(true)
    } else {
        anyhow::bail!(
            "proxy failed to start. Check logs at {}",
            log_path.display()
        );
    }
}

// ---------------------------------------------------------------------------
// Proxy status / control
// ---------------------------------------------------------------------------

/// Check if a nsl proxy is running by sending an HTTP HEAD request.
pub async fn is_proxy_running(port: u16) -> bool {
    let client = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
        .build_http();
    let uri = format!("http://127.0.0.1:{}/", port);
    let req = match Request::builder()
        .method("HEAD")
        .uri(&uri)
        .body(Full::new(Bytes::new()))
    {
        Ok(r) => r,
        Err(_) => return false,
    };
    match client.request(req).await {
        Ok(resp) => resp.headers().contains_key(NSL_HEADER),
        Err(_) => false,
    }
}

/// Wait for the proxy to become ready, polling up to `max_attempts` times.
pub async fn wait_for_proxy(port: u16, max_attempts: u32, interval_ms: u64) -> bool {
    for _ in 0..max_attempts {
        if is_proxy_running(port).await {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(interval_ms)).await;
    }
    false
}

/// Stop a running proxy by reading PID from state dir and terminating it.
pub fn stop_proxy(state_dir: &Path) -> anyhow::Result<()> {
    let pid_path = state_dir.join("proxy.pid");
    if !pid_path.exists() {
        anyhow::bail!("proxy is not running (no PID file found)");
    }

    let pid_str = fs::read_to_string(&pid_path)?;
    let pid: u32 = pid_str
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid PID in {}", pid_path.display()))?;

    if !crate::platform::is_process_alive(pid) {
        cleanup_lifecycle_files(state_dir);
        anyhow::bail!(
            "proxy process {} is not running (stale PID file cleaned up)",
            pid
        );
    }

    crate::platform::terminate_process(pid)?;

    // Wait briefly for process to exit, then clean up
    for _ in 0..20 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if !crate::platform::is_process_alive(pid) {
            cleanup_lifecycle_files(state_dir);
            tracing::info!("proxy stopped (PID {})", pid);
            return Ok(());
        }
    }

    cleanup_lifecycle_files(state_dir);
    tracing::warn!("proxy (PID {}) may still be running", pid);
    Ok(())
}

// ---------------------------------------------------------------------------
// Logs
// ---------------------------------------------------------------------------

/// Show proxy daemon logs from state_dir/proxy.log.
pub async fn show_logs(
    state_dir: &Path,
    follow: bool,
    tail_lines: Option<usize>,
) -> anyhow::Result<()> {
    let log_path = state_dir.join("proxy.log");
    if !log_path.exists() {
        anyhow::bail!(
            "no log file found at {}. Is the proxy running in daemon mode?",
            log_path.display()
        );
    }

    let content = fs::read_to_string(&log_path)?;

    let output = match tail_lines {
        Some(n) => {
            let lines: Vec<&str> = content.lines().collect();
            let start = lines.len().saturating_sub(n);
            lines[start..].join("\n")
        }
        None => content.clone(),
    };

    if !output.is_empty() {
        print!("{}", output);
        if !output.ends_with('\n') {
            println!();
        }
    }

    if !follow {
        return Ok(());
    }

    // Follow mode: poll for new content every 200ms
    use std::io::Write;
    let mut pos = content.len() as u64;

    loop {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let metadata = match fs::metadata(&log_path) {
            Ok(m) => m,
            Err(_) => continue,
        };

        let file_len = metadata.len();

        if file_len < pos {
            pos = 0;
        }

        if file_len > pos {
            let mut file = fs::File::open(&log_path)?;
            use std::io::{Read, Seek, SeekFrom};
            file.seek(SeekFrom::Start(pos))?;
            let mut buf = vec![0u8; (file_len - pos) as usize];
            file.read_exact(&mut buf)?;
            let text = String::from_utf8_lossy(&buf);
            print!("{}", text);
            std::io::stdout().flush()?;
            pos = file_len;
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stop_proxy_stale_pid_file() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path();

        fs::write(state_dir.join("proxy.pid"), "999999999").unwrap();
        fs::write(state_dir.join("proxy.port"), "1355").unwrap();

        let result = stop_proxy(state_dir);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("not running"));

        assert!(!state_dir.join("proxy.pid").exists());
        assert!(!state_dir.join("proxy.port").exists());
    }

    #[test]
    fn test_stop_proxy_no_pid_file() {
        let dir = tempfile::tempdir().unwrap();
        let result = stop_proxy(dir.path());
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("not running"));
    }

    #[tokio::test]
    async fn test_show_logs_no_log_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let result = show_logs(tmp.path(), false, None).await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("no log file found"),
            "expected 'no log file found' error, got: {}",
            err_msg
        );
    }

    #[tokio::test]
    async fn test_show_logs_reads_full_content() {
        let tmp = tempfile::TempDir::new().unwrap();
        let log_path = tmp.path().join("proxy.log");
        fs::write(&log_path, "line1\nline2\nline3\n").unwrap();

        let result = show_logs(tmp.path(), false, None).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_show_logs_with_tail_lines() {
        let tmp = tempfile::TempDir::new().unwrap();
        let log_path = tmp.path().join("proxy.log");
        fs::write(&log_path, "line1\nline2\nline3\nline4\nline5\n").unwrap();

        let result = show_logs(tmp.path(), false, Some(2)).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_show_logs_tail_more_than_available() {
        let tmp = tempfile::TempDir::new().unwrap();
        let log_path = tmp.path().join("proxy.log");
        fs::write(&log_path, "line1\nline2\n").unwrap();

        let result = show_logs(tmp.path(), false, Some(100)).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_show_logs_empty_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let log_path = tmp.path().join("proxy.log");
        fs::write(&log_path, "").unwrap();

        let result = show_logs(tmp.path(), false, None).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_startup_lock_releases_on_drop() {
        let tmp = tempfile::tempdir().unwrap();
        let lock_path = startup_lock_path(tmp.path());

        let lock = acquire_startup_lock_with_params(
            tmp.path(),
            1,
            Duration::from_millis(1),
            Duration::from_secs(60),
        )
        .await
        .unwrap();
        assert!(lock_path.exists());

        drop(lock);
        assert!(!lock_path.exists());
    }

    #[tokio::test]
    async fn test_startup_lock_serializes_waiters() {
        let tmp = tempfile::tempdir().unwrap();
        let first = acquire_startup_lock_with_params(
            tmp.path(),
            1,
            Duration::from_millis(1),
            Duration::from_secs(60),
        )
        .await
        .unwrap();

        let path = tmp.path().to_path_buf();
        let waiter = tokio::spawn(async move {
            acquire_startup_lock_with_params(
                &path,
                20,
                Duration::from_millis(5),
                Duration::from_secs(60),
            )
            .await
        });

        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!waiter.is_finished());

        drop(first);

        let second = waiter.await.unwrap().unwrap();
        drop(second);
    }

    #[tokio::test]
    async fn test_startup_lock_reclaims_stale_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let lock_path = startup_lock_path(tmp.path());
        fs::create_dir(&lock_path).unwrap();

        tokio::time::sleep(Duration::from_millis(10)).await;

        let lock = acquire_startup_lock_with_params(
            tmp.path(),
            2,
            Duration::from_millis(1),
            Duration::ZERO,
        )
        .await
        .unwrap();
        assert!(lock_path.exists());
        drop(lock);
    }
}

//! `nsld` — the nsl tunnel server daemon.
//!
//! Orchestrates three things:
//!   1. QUIC tunnel server (from the `tunnel` crate).
//!   2. Public HTTP/HTTPS listener that routes `Host`/SNI -> tenant
//!      session -> QUIC bi-stream.
//!   3. ACME manager that issues and renews a per-tenant wildcard cert
//!      (covering apex + `*.apex`) on demand via a DNS-01 `httpreq`
//!      webhook.
//!
//! All on-disk state lives under one `state_dir` (default `./data`
//! under the current working directory).

mod acme;
mod default_cert;
mod forward_auth;
mod public;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use clap::{Parser, Subcommand};
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::acme::{AcmeConfig, AcmeManager, AcmeResolver, CertCache};
use crate::default_cert::DefaultCertManager;

const DEFAULT_LISTEN: &str = ":443";
const DEFAULT_PUBLIC_HTTPS: &str = ":443";
const DEFAULT_PUBLIC_HTTP: &str = ":80";

#[derive(Parser)]
#[command(name = "nsld", version, about = "nsl tunnel server daemon")]
struct Cli {
    /// Path to config file (TOML). If unset, searched at
    /// `$NSLD_CONFIG` -> `{state_dir}/config.toml` -> `nsld.toml`.
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,

    /// Base directory for all persistent state (identity, tokens, acme).
    /// Overrides the config's `state_dir`. Defaults to `./data` under
    /// the current working directory.
    #[arg(long, global = true)]
    state_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the tunnel server (default)
    Serve {
        /// QUIC listen address. Defaults to `:443`.
        #[arg(short, long)]
        listen: Option<String>,
        /// Base domain this server is authoritative for (e.g. `nsl.example.com`).
        #[arg(long)]
        base_domain: Option<String>,
    },

    /// Print the resolved configuration and exit
    Config,
}

#[derive(Debug, Default, Deserialize, Clone)]
struct RawConfig {
    state_dir: Option<String>,
    server: Option<RawServer>,
    public: Option<RawPublic>,
    acme: Option<RawAcme>,
    forward_auth: Option<RawForwardAuth>,
}

#[derive(Debug, Default, Deserialize, Clone)]
struct RawServer {
    listen: Option<String>,
    base_domain: Option<String>,
}

#[derive(Debug, Default, Deserialize, Clone)]
struct RawPublic {
    https_listen: Option<String>,
    http_listen: Option<String>,
}

#[derive(Debug, Default, Deserialize, Clone)]
struct RawAcme {
    enable: Option<bool>,
    contact_email: Option<String>,
    directory: Option<String>,
    httpreq_url: Option<String>,
    httpreq_username: Option<String>,
    httpreq_password: Option<String>,
    propagation_wait_secs: Option<u64>,
    renewal_threshold_days: Option<i64>,
}

#[derive(Debug, Default, Deserialize, Clone)]
struct RawForwardAuth {
    enable: Option<bool>,
    address: Option<String>,
    #[serde(default)]
    request_headers: Vec<String>,
    #[serde(default)]
    response_headers: Vec<String>,
    #[serde(default)]
    bypass_prefixes: Vec<String>,
    timeout_secs: Option<u64>,
    tls_verify: Option<bool>,
}

/// Resolved forward-auth settings. `None` means the gate is disabled
/// and public requests flow through without the pre-check.
#[derive(Debug, Clone)]
struct ForwardAuthResolved {
    address: String,
    request_headers: Vec<String>,
    response_headers: Vec<String>,
    bypass_prefixes: Vec<String>,
    timeout_secs: Option<u64>,
    tls_verify: bool,
}

#[derive(Debug, Clone)]
struct Resolved {
    state_dir: PathBuf,
    server: tunnel::ServerTunnel,
    public_https_listen: String,
    public_http_listen: String,
    acme: AcmeConfig,
    forward_auth: Option<ForwardAuthResolved>,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();

    let cli = Cli::parse();

    let state_dir_override = cli.state_dir.clone();
    let raw = load_raw_config(cli.config.as_deref(), state_dir_override.as_deref())?;

    let rt = tokio::runtime::Runtime::new().context("creating tokio runtime")?;
    rt.block_on(async move {
        match cli.command {
            Command::Serve {
                listen,
                base_domain,
            } => {
                let resolved = resolve(&raw, state_dir_override, listen, base_domain)?;
                run_all(resolved).await
            }
            Command::Config => {
                match resolve(&raw, state_dir_override, None, None) {
                    Ok(r) => print_resolved(&r),
                    Err(e) => println!("nsld config incomplete: {e}"),
                }
                Ok(())
            }
        }
    })
}

async fn run_all(cfg: Resolved) -> Result<()> {
    let cancel = CancellationToken::new();
    let registry = tunnel::registry::SessionRegistry::new();
    let cert_cache: CertCache = acme::new_cache();

    // ACME manager — may or may not be enabled.
    let acme_mgr = if cfg.acme.enable {
        let mgr = AcmeManager::start(cfg.acme.clone(), cert_cache.clone())
            .await
            .context("starting ACME manager")?;
        mgr.spawn_renewal(cancel.clone());
        Some(mgr)
    } else {
        None
    };

    // Private-CA fallback runs alongside ACME whenever HTTPS is
    // enabled. The shared CertCache's priority rule ("ACME insert wins,
    // default or_insert-only") ensures ACME certs beat the default for
    // the same tenant, while the default still covers the window before
    // an ACME issuance completes — or permanently if ACME keeps failing.
    let default_cert_mgr = if !cfg.public_https_listen.is_empty() {
        let mgr = DefaultCertManager::start(cfg.state_dir.clone(), cert_cache.clone())
            .context("starting default self-signed cert manager")?;
        Some(mgr)
    } else {
        None
    };

    // Session hook fires on every successful tenant handshake. Prime
    // both managers: the default fills the slot immediately (local
    // signing, microseconds) so HTTPS works the moment the first
    // public request arrives; ACME then overwrites with a trusted
    // cert asynchronously if/when its issuance completes.
    let hook: Option<tunnel::server::SessionHook> = {
        let acme = acme_mgr.clone();
        let default_cert = default_cert_mgr.clone();
        if acme.is_none() && default_cert.is_none() {
            None
        } else {
            Some(Arc::new(move |domain: &str| {
                if let Some(mgr) = &default_cert {
                    mgr.ensure_cert(domain);
                }
                if let Some(mgr) = &acme {
                    mgr.ensure_cert(domain);
                }
            }))
        }
    };

    // --- tunnel server ---
    let server_cfg = cfg.server.clone();
    let server_registry = registry.clone();
    let server_cancel = cancel.clone();
    let server_hook = hook.clone();
    let server_task = tokio::spawn(async move {
        tunnel::server::run_with_cancel(server_cfg, server_registry, server_hook, server_cancel)
            .await
    });

    // Forward-auth gate client (optional). When Some, wired into the
    // Route-mode public listeners; Redirect-mode listeners skip it.
    let forward_auth_client: Option<Arc<forward_auth::ForwardAuthClient>> = match &cfg.forward_auth
    {
        Some(settings) => {
            let fwd_cfg = forward_auth::ForwardAuthConfig::new(
                settings.address.clone(),
                settings.response_headers.clone(),
                settings.request_headers.clone(),
                settings.timeout_secs,
                settings.bypass_prefixes.clone(),
                settings.tls_verify,
            )
            .context("invalid forward_auth config")?;
            let client = forward_auth::ForwardAuthClient::new(fwd_cfg)
                .context("building forward-auth client")?;
            tracing::info!(
                address = %settings.address,
                "forward auth enabled; every public request gated by external authorization"
            );
            Some(Arc::new(client))
        }
        None => {
            // Loud warning: untrusted internet traffic is reaching
            // tenants with no gate. Acceptable for local-dev or when
            // an external reverse proxy is doing auth in front; not
            // acceptable as a default production posture.
            tracing::warn!(
                "[forward_auth].enable = false: nsld is serving public traffic with NO \
                 authentication. Either put a reverse proxy (Traefik / Caddy / nginx) in \
                 front of nsld doing auth, or enable [forward_auth] — see SECURITY.md."
            );
            None
        }
    };

    // --- public listeners ---
    let mut public_tasks = Vec::new();
    // HTTPS runs whenever the default manager (which always exists when
    // https_listen is set) or the ACME manager is alive. ACME alone
    // would still serve the cache but the default covers cold-start.
    let https_active = default_cert_mgr.is_some() || acme_mgr.is_some();
    if https_active {
        // HTTPS with the shared SNI → tenant resolver. The cache is
        // filled by whichever cert manager is active.
        let resolver = Arc::new(AcmeResolver::new(
            cert_cache.clone(),
            cfg.server.base_domain.clone(),
        ));
        let https_cancel = cancel.clone();
        let https_listen = cfg.public_https_listen.clone();
        let https_registry = registry.clone();
        let https_fwd = forward_auth_client.clone();
        public_tasks.push(tokio::spawn(async move {
            if let Err(e) = public::run_https(
                &https_listen,
                https_registry,
                resolver,
                https_fwd,
                https_cancel,
            )
            .await
            {
                tracing::error!(error = %e, "public HTTPS listener exited");
            }
        }));

        // Plain :80 redirects to HTTPS. No forward-auth here — the
        // client is about to be 301'd anyway.
        if !cfg.public_http_listen.is_empty() {
            let redirect_cancel = cancel.clone();
            let redirect_listen = cfg.public_http_listen.clone();
            let redirect_registry = registry.clone();
            public_tasks.push(tokio::spawn(async move {
                if let Err(e) = public::run_plain(
                    &redirect_listen,
                    redirect_registry,
                    public::PlainMode::Redirect,
                    None,
                    redirect_cancel,
                )
                .await
                {
                    tracing::error!(error = %e, "HTTP redirect listener exited");
                }
            }));
        }
    } else {
        // No TLS at all — plain HTTP routing on :80.
        let plain_cancel = cancel.clone();
        let plain_listen = cfg.public_http_listen.clone();
        let plain_registry = registry.clone();
        let plain_fwd = forward_auth_client.clone();
        public_tasks.push(tokio::spawn(async move {
            if let Err(e) = public::run_plain(
                &plain_listen,
                plain_registry,
                public::PlainMode::Route,
                plain_fwd,
                plain_cancel,
            )
            .await
            {
                tracing::error!(error = %e, "public HTTP listener exited");
            }
        }));
    }

    // --- shutdown wiring ---
    #[cfg(unix)]
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("registering SIGTERM handler")?;

    let shutdown = async move {
        let ctrl_c = tokio::signal::ctrl_c();
        #[cfg(unix)]
        tokio::select! {
            _ = ctrl_c => tracing::info!("received SIGINT, shutting down"),
            _ = sigterm.recv() => tracing::info!("received SIGTERM, shutting down"),
        }
        #[cfg(not(unix))]
        {
            let _ = ctrl_c.await;
            tracing::info!("received Ctrl+C, shutting down");
        }
    };

    tokio::pin!(server_task);
    tokio::select! {
        res = &mut server_task => {
            match res {
                Ok(Ok(())) => {}
                Ok(Err(e)) => return Err(e),
                Err(e) => return Err(anyhow!("server task panicked: {e}")),
            }
        }
        _ = shutdown => {
            cancel.cancel();
            match tokio::time::timeout(std::time::Duration::from_secs(5), server_task).await {
                Ok(Ok(Ok(()))) => {}
                Ok(Ok(Err(e))) => return Err(e),
                Ok(Err(e)) => return Err(anyhow!("server task panicked: {e}")),
                Err(_) => tracing::warn!("server did not stop within 5s"),
            }
        }
    }
    for t in public_tasks {
        t.abort();
    }
    drop(acme_mgr);
    drop(default_cert_mgr);
    Ok(())
}

fn resolve(
    raw: &RawConfig,
    state_dir_override: Option<PathBuf>,
    listen_override: Option<String>,
    base_domain_override: Option<String>,
) -> Result<Resolved> {
    let state_dir = state_dir_override
        .or_else(|| raw.state_dir.as_deref().map(PathBuf::from))
        .unwrap_or_else(default_state_dir);

    std::fs::create_dir_all(&state_dir)
        .with_context(|| format!("creating state_dir {}", state_dir.display()))?;

    let server_raw = raw.server.clone().unwrap_or_default();
    let listen = listen_override
        .filter(|s| !s.trim().is_empty())
        .or_else(|| server_raw.listen.filter(|s| !s.trim().is_empty()))
        .or_else(|| Some(DEFAULT_LISTEN.to_string()));
    let base_domain = base_domain_override
        .filter(|s| !s.trim().is_empty())
        .or(server_raw.base_domain)
        .ok_or_else(|| anyhow!("server.base_domain is required"))?;

    let tokens_file = state_dir.join("tokens.toml");
    let identity_path = state_dir.join("identity.pem");
    let server = tunnel::ServerTunnel::new(
        listen,
        base_domain,
        tokens_file.to_string_lossy().into_owned(),
        identity_path,
    )?;

    let public_raw = raw.public.clone().unwrap_or_default();
    let acme_raw = raw.acme.clone().unwrap_or_default();
    let acme_enable = acme_raw.enable.unwrap_or(false);

    // When ACME is on we assume the operator wants HTTPS on :443 by
    // default. When ACME is off, leave https_listen empty unless the
    // operator explicitly opts into the self-signed default-cert path.
    let public_https_listen = public_raw
        .https_listen
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| {
            if acme_enable {
                DEFAULT_PUBLIC_HTTPS.to_string()
            } else {
                String::new()
            }
        });
    let public_http_listen = public_raw
        .http_listen
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_PUBLIC_HTTP.to_string());

    // Env fallbacks mirror lego's conventions so operators can reuse
    // existing credentials without touching the config file.
    let username = acme_raw
        .httpreq_username
        .or_else(|| std::env::var("HTTPREQ_USERNAME").ok());
    let password = acme_raw
        .httpreq_password
        .or_else(|| std::env::var("HTTPREQ_PASSWORD").ok());
    let acme = AcmeConfig {
        enable: acme_enable,
        contact_email: acme_raw.contact_email.unwrap_or_default(),
        directory: acme_raw
            .directory
            .unwrap_or_else(|| "https://acme-v02.api.letsencrypt.org/directory".into()),
        httpreq_url: acme_raw
            .httpreq_url
            .or_else(|| std::env::var("HTTPREQ_ENDPOINT").ok())
            .unwrap_or_default(),
        httpreq_username: username,
        httpreq_password: password,
        propagation_wait_secs: acme_raw.propagation_wait_secs.unwrap_or(30),
        renewal_threshold_days: acme_raw.renewal_threshold_days.unwrap_or(30),
        store_root: state_dir.join("acme"),
    };
    acme.validate()?;

    // Forward auth defaults to off. Only materialises when the operator
    // explicitly flips `[forward_auth].enable = true` AND supplies a
    // non-empty address.
    let forward_auth = resolve_forward_auth(raw.forward_auth.as_ref());

    Ok(Resolved {
        state_dir,
        server,
        public_https_listen,
        public_http_listen,
        acme,
        forward_auth,
    })
}

fn resolve_forward_auth(raw: Option<&RawForwardAuth>) -> Option<ForwardAuthResolved> {
    let fwd = raw?;
    if !fwd.enable.unwrap_or(false) {
        return None;
    }
    let address = fwd.address.as_deref().map(str::trim).unwrap_or("");
    if address.is_empty() {
        tracing::warn!("[forward_auth].enable = true but address is empty; gate stays off");
        return None;
    }
    Some(ForwardAuthResolved {
        address: address.to_string(),
        request_headers: fwd.request_headers.clone(),
        response_headers: fwd.response_headers.clone(),
        bypass_prefixes: fwd.bypass_prefixes.clone(),
        timeout_secs: fwd.timeout_secs,
        tls_verify: fwd.tls_verify.unwrap_or(true),
    })
}

fn default_state_dir() -> PathBuf {
    // `./data` under the current working directory. The daemon is
    // already `nsld`, so the nested prefix would be redundant.
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("data")
}

fn load_raw_config(explicit: Option<&Path>, state_dir_hint: Option<&Path>) -> Result<RawConfig> {
    if let Some(path) = explicit {
        return read_toml(path);
    }
    if let Ok(p) = std::env::var("NSLD_CONFIG") {
        let path = PathBuf::from(p);
        if path.is_file() {
            return read_toml(&path);
        }
    }
    let candidates: Vec<PathBuf> = [
        state_dir_hint
            .map(|p| p.join("config.toml"))
            .unwrap_or_else(|| default_state_dir().join("config.toml")),
        PathBuf::from("nsld.toml"),
    ]
    .into_iter()
    .collect();
    for path in candidates {
        if path.is_file() {
            return read_toml(&path);
        }
    }
    Ok(RawConfig::default())
}

fn read_toml(path: &Path) -> Result<RawConfig> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("reading config file {}", path.display()))?;
    toml::from_str(&content).with_context(|| format!("parsing config file {}", path.display()))
}

fn print_resolved(r: &Resolved) {
    println!("nsld config");
    println!("  state_dir:     {}", r.state_dir.display());
    println!("  server.listen: {}", r.server.listen);
    println!("  server.base_domain: {}", r.server.base_domain);
    println!("  server.tokens_file: {}", r.server.tokens_file);
    println!(
        "  server.identity_path: {}",
        r.server.identity_path.display()
    );
    let https_mode = if r.public_https_listen.is_empty() {
        "off"
    } else if r.acme.enable {
        "acme (+ default CA fallback)"
    } else {
        "default CA only"
    };
    let listen_display = if r.public_https_listen.is_empty() {
        "(off)"
    } else {
        r.public_https_listen.as_str()
    };
    println!("  public.https_listen: {listen_display}   [mode: {https_mode}]");
    println!("  public.http_listen:  {}", r.public_http_listen);
    println!("  acme.enable:   {}", r.acme.enable);
    if r.acme.enable {
        println!("    contact_email:   {}", r.acme.contact_email);
        println!("    directory:       {}", r.acme.directory);
        println!("    httpreq_url:     {}", r.acme.httpreq_url);
        let auth_state = match (&r.acme.httpreq_username, &r.acme.httpreq_password) {
            (Some(u), Some(_)) if !u.is_empty() => format!("Basic ({})", u),
            _ => "none".into(),
        };
        println!("    httpreq_auth:    {}", auth_state);
        println!("    propagation:     {}s", r.acme.propagation_wait_secs);
        println!("    renew_threshold: {}d", r.acme.renewal_threshold_days);
        println!("    store_root:      {}", r.acme.store_root.display());
    }
    match &r.forward_auth {
        Some(fa) => {
            println!("  forward_auth:  enabled → {}", fa.address);
            if !fa.bypass_prefixes.is_empty() {
                println!("    bypass_prefixes:  {}", fa.bypass_prefixes.join(", "));
            }
            if !fa.response_headers.is_empty() {
                println!("    response_headers: {}", fa.response_headers.join(", "));
            }
            if !fa.tls_verify {
                println!("    tls_verify:       off  (self-signed auth endpoint)");
            }
        }
        None => println!("  forward_auth:  disabled"),
    }
}

//! QUIC-based reverse tunnel: expose local apps via a remote server.
//!
//! Architecture:
//! - Single long-lived QUIC connection from client -> server.
//! - Control stream (first bi-stream): Hello/HelloAck for auth, Ping/Pong
//!   for keepalive.
//! - Data streams: server opens one bi-stream per public request, raw TCP
//!   bytes are forwarded to the client's local proxy.
//! - Server routes by Host suffix: `*.<tenant_domain>` -> session.
//!
//! Token issuance is out-of-band. A server-side TOML file lists
//! `(domain, key)` pairs that are allowed to authenticate.

pub mod client;
pub mod config;
pub mod constants;
pub mod protocol;
pub mod registry;
pub mod server;
pub mod tls;
pub mod tokens;

pub use config::{ClientTunnel, ServerTunnel};

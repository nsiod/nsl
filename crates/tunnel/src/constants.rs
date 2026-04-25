//! Shared timing and protocol constants.

/// Max time to complete the full handshake (Hello -> Challenge ->
/// AuthResponse -> HelloAck). Applies on both client and server.
pub const HANDSHAKE_TIMEOUT_SECS: u64 = 10;

/// Server-advertised keepalive interval (seconds). The client sends
/// `Ping` this often and the server replies with `Pong`.
pub const KEEPALIVE_SECS: u32 = 15;

/// Upper bound on how often the client pings — prevents pathological
/// configs from flooding. Real interval is `min(PING_INTERVAL_CAP_SECS,
/// server_keepalive)`.
pub const PING_INTERVAL_CAP_SECS: u64 = 30;

/// Nonce length in bytes for the challenge-response handshake.
pub const NONCE_LEN: usize = 32;

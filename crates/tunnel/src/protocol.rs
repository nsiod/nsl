//! Tunnel wire protocol.
//!
//! The control stream carries length-prefixed bincode-encoded frames.
//! Data streams are raw TCP byte streams (server opens them, one per
//! incoming public HTTP request; no framing).
//!
//! Handshake (v4): server-authoritative identity + mutual HMAC.
//!   client -> Hello { version, client_id, client_nonce }
//!   server -> Challenge { server_nonce, server_proof = HMAC(key, client_nonce) }
//!   (client verifies server_proof; aborts if mismatch -- server auth)
//!   client -> AuthResponse { digest = HMAC(key, server_nonce) }
//!   server -> HelloAck { session_id, assigned_domain, keepalive_secs } or HelloErr
//!
//! The client only advertises an opaque `client_id` (e.g. `"alice"`).
//! The server looks up the token table, decides which public domain
//! this client is authoritative for, and returns it in `HelloAck`. The
//! client then adds the assigned domain to its local proxy-allowed
//! domains list so route registration works out of the box.

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::constants::NONCE_LEN;

pub const PROTOCOL_VERSION: u32 = 4;
/// ALPN advertised on both sides. We piggy-back on HTTP/3's `h3` label so
/// passive DPI sees a generic HTTP/3 flow rather than a custom tunnel.
/// The `PROTOCOL_VERSION` field in the Hello frame still pins wire
/// compatibility, and any real HTTP/3 client probing us will get garbage
/// at the application-framing layer and disconnect.
pub const ALPN: &[u8] = b"h3";
pub const MAX_FRAME_BYTES: usize = 64 * 1024;

pub const DIGEST_LEN: usize = 32;

pub type Nonce = [u8; NONCE_LEN];
pub type Digest = [u8; DIGEST_LEN];

/// Control frames on the control stream. Bincode-encoded, length-prefixed
/// with a u32 big-endian length.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ControlFrame {
    /// Client -> server: announce the client's opaque identifier (not
    /// its public domain — the server decides that) and supply the
    /// nonce the server must HMAC to prove it knows the shared key.
    Hello {
        version: u32,
        client_id: String,
        #[serde(with = "serde_nonce")]
        client_nonce: Nonce,
    },
    /// Server -> client: proof that the server knows the shared key
    /// (`server_proof = HMAC(key, client_nonce)`) plus a fresh
    /// `server_nonce` the client must HMAC in return.
    Challenge {
        #[serde(with = "serde_nonce")]
        server_nonce: Nonce,
        #[serde(with = "serde_digest")]
        server_proof: Digest,
    },
    /// Client -> server: HMAC-SHA256(key, server_nonce).
    AuthResponse {
        #[serde(with = "serde_digest")]
        digest: Digest,
    },
    /// Server -> client: authentication succeeded. `assigned_domain`
    /// is the public tenant domain the server routes for this client.
    HelloAck {
        session_id: String,
        assigned_domain: String,
        /// Server-advertised heartbeat interval in seconds.
        keepalive_secs: u32,
    },
    /// Server -> client: authentication / protocol error. Server closes.
    HelloErr { reason: String },
    /// Either side: keepalive ping.
    Ping,
    /// Either side: keepalive pong.
    Pong,
}

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("frame too large: {size} > {max}")]
    FrameTooLarge { size: usize, max: usize },
    #[error("frame codec error: {0}")]
    Codec(#[from] bincode::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("unexpected eof reading frame")]
    UnexpectedEof,
}

pub fn encode_frame(frame: &ControlFrame) -> Result<Vec<u8>, ProtocolError> {
    let payload = bincode::serialize(frame)?;
    if payload.len() > MAX_FRAME_BYTES {
        return Err(ProtocolError::FrameTooLarge {
            size: payload.len(),
            max: MAX_FRAME_BYTES,
        });
    }
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(&payload);
    Ok(out)
}

pub fn decode_frame(buf: &[u8]) -> Result<ControlFrame, ProtocolError> {
    Ok(bincode::deserialize(buf)?)
}

/// Read a length-prefixed frame from an async reader.
pub async fn read_frame<R>(reader: &mut R) -> Result<ControlFrame, ProtocolError>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::UnexpectedEof {
            ProtocolError::UnexpectedEof
        } else {
            ProtocolError::Io(e)
        }
    })?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(ProtocolError::FrameTooLarge {
            size: len,
            max: MAX_FRAME_BYTES,
        });
    }
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).await?;
    decode_frame(&buf)
}

/// Write a frame with u32 length prefix.
pub async fn write_frame<W>(writer: &mut W, frame: &ControlFrame) -> Result<(), ProtocolError>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let bytes = encode_frame(frame)?;
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

/// HMAC-SHA256(key, nonce) -> 32-byte digest.
pub fn sign(key: &[u8], nonce: &Nonce) -> Digest {
    use ring::hmac;
    let k = hmac::Key::new(hmac::HMAC_SHA256, key);
    let tag = hmac::sign(&k, nonce);
    let mut out = [0u8; DIGEST_LEN];
    out.copy_from_slice(tag.as_ref());
    out
}

/// Constant-time verify of `digest == HMAC-SHA256(key, nonce)`.
pub fn verify(key: &[u8], nonce: &Nonce, digest: &Digest) -> bool {
    use ring::hmac;
    let k = hmac::Key::new(hmac::HMAC_SHA256, key);
    hmac::verify(&k, nonce, digest).is_ok()
}

/// Random nonce from the system RNG.
pub fn random_nonce() -> Result<Nonce, ring::error::Unspecified> {
    use ring::rand::{SecureRandom, SystemRandom};
    let rng = SystemRandom::new();
    let mut nonce = [0u8; NONCE_LEN];
    rng.fill(&mut nonce)?;
    Ok(nonce)
}

// Serde doesn't derive for `[u8; N]` on stable in a way that survives
// bincode (it tries to encode as tuple). Encode as a byte array instead.
mod serde_nonce {
    use super::NONCE_LEN;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(v: &[u8; NONCE_LEN], s: S) -> Result<S::Ok, S::Error> {
        serde_bytes::Bytes::new(v).serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; NONCE_LEN], D::Error> {
        let bytes: &serde_bytes::Bytes = Deserialize::deserialize(d)?;
        let slice: &[u8] = bytes;
        if slice.len() != NONCE_LEN {
            return Err(serde::de::Error::custom("wrong nonce length"));
        }
        let mut out = [0u8; NONCE_LEN];
        out.copy_from_slice(slice);
        Ok(out)
    }
}

mod serde_digest {
    use super::DIGEST_LEN;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(v: &[u8; DIGEST_LEN], s: S) -> Result<S::Ok, S::Error> {
        serde_bytes::Bytes::new(v).serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; DIGEST_LEN], D::Error> {
        let bytes: &serde_bytes::Bytes = Deserialize::deserialize(d)?;
        let slice: &[u8] = bytes;
        if slice.len() != DIGEST_LEN {
            return Err(serde::de::Error::custom("wrong digest length"));
        }
        let mut out = [0u8; DIGEST_LEN];
        out.copy_from_slice(slice);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_roundtrip() {
        let client_nonce = [5u8; NONCE_LEN];
        let f = ControlFrame::Hello {
            version: PROTOCOL_VERSION,
            client_id: "alice".to_string(),
            client_nonce,
        };
        let encoded = encode_frame(&f).unwrap();
        let decoded = decode_frame(&encoded[4..]).unwrap();
        match decoded {
            ControlFrame::Hello {
                client_id,
                client_nonce: n,
                ..
            } => {
                assert_eq!(client_id, "alice");
                assert_eq!(n, client_nonce);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn challenge_roundtrip() {
        let server_nonce = [7u8; NONCE_LEN];
        let server_proof = [9u8; DIGEST_LEN];
        let f = ControlFrame::Challenge {
            server_nonce,
            server_proof,
        };
        let encoded = encode_frame(&f).unwrap();
        let decoded = decode_frame(&encoded[4..]).unwrap();
        match decoded {
            ControlFrame::Challenge {
                server_nonce: n,
                server_proof: p,
            } => {
                assert_eq!(n, server_nonce);
                assert_eq!(p, server_proof);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn auth_response_roundtrip() {
        let digest = [3u8; DIGEST_LEN];
        let f = ControlFrame::AuthResponse { digest };
        let encoded = encode_frame(&f).unwrap();
        let decoded = decode_frame(&encoded[4..]).unwrap();
        match decoded {
            ControlFrame::AuthResponse { digest: d } => assert_eq!(d, digest),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn sign_verify_roundtrip() {
        let key = b"sekret";
        let nonce = random_nonce().unwrap();
        let d = sign(key, &nonce);
        assert!(verify(key, &nonce, &d));
        assert!(!verify(b"other", &nonce, &d));
        let bad_nonce = [9u8; NONCE_LEN];
        assert!(!verify(key, &bad_nonce, &d));
    }

    #[test]
    fn length_prefix_is_big_endian() {
        let f = ControlFrame::Ping;
        let encoded = encode_frame(&f).unwrap();
        let len = u32::from_be_bytes([encoded[0], encoded[1], encoded[2], encoded[3]]);
        assert_eq!(len as usize + 4, encoded.len());
    }

    #[tokio::test]
    async fn async_read_write_roundtrip() {
        let (mut client, mut server) = tokio::io::duplex(1024);
        let frame = ControlFrame::Hello {
            version: PROTOCOL_VERSION,
            client_id: "alice".to_string(),
            client_nonce: [0u8; NONCE_LEN],
        };
        tokio::try_join!(async { write_frame(&mut client, &frame).await }, async {
            let got = read_frame(&mut server).await?;
            match got {
                ControlFrame::Hello { client_id, .. } => {
                    assert_eq!(client_id, "alice");
                    Ok(())
                }
                _ => panic!("wrong variant"),
            }
        })
        .unwrap();
    }

    #[tokio::test]
    async fn read_frame_rejects_oversized_length() {
        let (mut client, mut server) = tokio::io::duplex(1024);
        let fake_len = (MAX_FRAME_BYTES as u32 + 1).to_be_bytes();
        tokio::io::AsyncWriteExt::write_all(&mut client, &fake_len)
            .await
            .unwrap();
        let err = read_frame(&mut server).await.unwrap_err();
        matches!(err, ProtocolError::FrameTooLarge { .. });
    }

    #[test]
    fn hello_ack_roundtrip() {
        let f = ControlFrame::HelloAck {
            session_id: "s1".to_string(),
            assigned_domain: "alice.nsl.example.com".to_string(),
            keepalive_secs: 15,
        };
        let encoded = encode_frame(&f).unwrap();
        let decoded = decode_frame(&encoded[4..]).unwrap();
        match decoded {
            ControlFrame::HelloAck {
                session_id,
                assigned_domain,
                keepalive_secs,
            } => {
                assert_eq!(session_id, "s1");
                assert_eq!(assigned_domain, "alice.nsl.example.com");
                assert_eq!(keepalive_secs, 15);
            }
            _ => panic!("wrong variant"),
        }
    }
}

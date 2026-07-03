//! Client/hub wire contract.
//!
//! The secret key is the *write* capability: a client presents it to push frames.
//! The session id is `base64url(sha256(key))` — a one-way hash, so it's a safe
//! *read* capability to embed in a viewer URL: you can render the session from the
//! id but cannot recover the key (and thus cannot push) from it.

use sha2::{Digest, Sha256};
use std::fmt::Write as _;

/// Header carrying the secret key on `/register` and `/frame`.
pub const KEY_HEADER: &str = "x-tmuxsnitch-key";

/// Underivable session id for a secret key: lowercase hex sha256. Hex (never `-`)
/// so the id is safe as a CLI value (`--allow`) and in a URL path.
pub fn session_id(key: &str) -> String {
    Sha256::digest(key.as_bytes())
        .iter()
        .fold(String::with_capacity(64), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// Upper bound on a single streamed frame; guards the hub against a corrupt or
/// hostile length prefix. A rendered screen is far smaller than this.
pub const MAX_FRAME: usize = 16 * 1024 * 1024;

/// Frame a payload for the streaming push body: `[u32 BE length][payload bytes]`.
/// A persistent POST carries a sequence of these, so the client never waits for a
/// per-frame HTTP response (that round-trip is what made the hub feel laggy).
pub fn frame_encode(payload: &str) -> Vec<u8> {
    let mut v = Vec::with_capacity(4 + payload.len());
    v.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    v.extend_from_slice(payload.as_bytes());
    v
}

/// Pull every complete frame out of `buf`, leaving any partial trailing frame.
/// `Err` means a length prefix exceeded [`MAX_FRAME`] — the stream is corrupt and
/// the caller should drop the connection.
pub fn frame_drain(buf: &mut Vec<u8>) -> Result<Vec<String>, ()> {
    let mut out = Vec::new();
    let mut pos = 0;
    while buf.len() - pos >= 4 {
        let len = u32::from_be_bytes([buf[pos], buf[pos + 1], buf[pos + 2], buf[pos + 3]]) as usize;
        if len > MAX_FRAME {
            return Err(());
        }
        if buf.len() - pos - 4 < len {
            break; // frame not fully arrived yet
        }
        let start = pos + 4;
        out.push(String::from_utf8_lossy(&buf[start..start + len]).into_owned());
        pos = start + len;
    }
    buf.drain(..pos);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_is_deterministic_and_hides_key() {
        let key = "correct horse battery staple";
        let id = session_id(key);
        assert_eq!(id, session_id(key), "same key -> same id");
        assert_ne!(id, session_id("other"), "different key -> different id");
        assert!(!id.contains(key), "id must not leak the key");
        assert_eq!(id.len(), 64, "sha256 -> 64 hex chars: {id}");
        assert!(
            id.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
            "id must be lowercase hex (no '-', CLI/URL safe): {id}"
        );
    }

    #[test]
    fn framing_roundtrips_and_keeps_partials() {
        // Two frames concatenated, plus a partial third, arriving in one buffer.
        let mut buf = proto_frames(&["<a>", "hello 世界"]);
        let partial = frame_encode("later");
        buf.extend_from_slice(&partial[..3]); // only part of the length prefix

        let frames = frame_drain(&mut buf).unwrap();
        assert_eq!(frames, vec!["<a>".to_string(), "hello 世界".to_string()]);
        assert_eq!(buf, &partial[..3], "partial frame must stay buffered");

        // A bogus oversized length is rejected.
        let mut bad = (u32::MAX).to_be_bytes().to_vec();
        bad.push(0);
        assert!(frame_drain(&mut bad).is_err());
    }

    fn proto_frames(payloads: &[&str]) -> Vec<u8> {
        payloads.iter().flat_map(|p| frame_encode(p)).collect()
    }
}

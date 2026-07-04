//! Client/hub wire contract.
//!
//! The secret key is the *write* capability: a client presents it to push frames.
//! The session id is `hex(argon2id(key))` — a one-way, memory-hard hash, so it's a
//! safe *read* capability to put in a viewer URL: you can render the session from
//! the id but cannot recover the key (and thus cannot push) from it.

use argon2::Argon2;
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;

/// One font the client serves to viewers, uploaded on `/register`. The hub stores
/// it per session and serves the bytes at `/s/<id>/fonts/<key>`, which the page's
/// `@font-face` references — so a viewer renders the glyphs even without the font
/// installed. Per-session storage means two clients' fonts never clash.
#[derive(Serialize, Deserialize)]
pub struct FontAsset {
    /// URL-safe id, unique within the session (the font's index).
    pub key: String,
    pub mime: String,
    /// base64 of the font file bytes.
    pub b64: String,
}

/// `/register` payload: the page CSS, the viewer template, and the fonts the CSS
/// references. The client renders locally, so it owns the template too and pushes
/// it here; the hub just fills it per request.
#[derive(Serialize, Deserialize)]
pub struct RegisterBody {
    pub css: String,
    /// Viewer HTML template with `{{style}}`/`{{screen}}`/`{{script}}` tokens.
    /// `default` (empty) so the hub falls back to its built-in for older clients.
    #[serde(default)]
    pub template: String,
    /// Render config JSON (colors + `symbol_map`) the hub injects into the page so
    /// the client's `viewer.js` resolves glyphs as the client would. `default`
    /// (empty) so the renderer falls back to its built-in defaults for older clients.
    #[serde(default)]
    pub render_cfg: String,
    #[serde(default)]
    pub fonts: Vec<FontAsset>,
}

/// Header carrying the secret key on `/register` and `/stream`.
pub const KEY_HEADER: &str = "x-shellglass-key";

/// Fixed application salt. The id must be a pure function of the secret (client
/// and hub derive it independently and must agree), so the salt can't be random
/// or per-hash — it's a constant. Memory-hardness, not the salt, is what slows a
/// brute force here.
///
/// The version suffix does double duty: it versions the derivation scheme AND
/// guards against protocol skew. Bump it on every breaking wire change — a
/// version-skewed client/hub pair then disagrees on `key → id`, so the stale side
/// fails loudly at `/register` (403, "register its session id") instead of
/// streaming frames the other side silently drops. The cost of a bump is that
/// operators re-run `print-id` and update `--allow` + view URLs; keys stay valid.
const SALT: &[u8] = b"shellglass/session-id/v2";

/// Underivable session id for a secret key: Argon2id (memory- and compute-hard)
/// rendered as lowercase hex. Memory-hardness makes brute-forcing a weak secret
/// from the public id expensive; with a strong random secret it's belt-and-
/// suspenders. Hex (never `-`) so the id is safe as a CLI value and URL path.
/// Cost is paid once per client connection (at `/register`), not per frame.
pub fn session_id(key: &str) -> String {
    let mut out = [0u8; 32];
    Argon2::default()
        .hash_password_into(key.as_bytes(), SALT, &mut out)
        .expect("argon2id with a fixed valid salt and output length cannot fail");
    out.iter().fold(String::with_capacity(64), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// Upper bound on a single streamed frame; guards the hub against a corrupt or
/// hostile length prefix. A rendered screen is far smaller than this.
pub const MAX_FRAME: usize = 16 * 1024 * 1024;

/// Frame a payload for the streaming push body: `[u32 BE length][payload bytes]`.
/// A persistent POST carries a sequence of these, so the client never waits for a
/// per-frame HTTP response (that round-trip is what made the hub feel laggy). The
/// payload is a [`crate::diff`] wire message — a full picture on (re)connect, then
/// rectangle deltas — which the hub applies to its own matrix and forwards to
/// viewers verbatim.
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
        assert_eq!(id.len(), 64, "argon2id 32 bytes -> 64 hex chars: {id}");
        assert!(
            id.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
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

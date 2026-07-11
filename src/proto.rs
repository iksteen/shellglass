//! Client/hub wire contract.
//!
//! The secret key is the *write* capability: a client presents it to push frames.
//! The session id is `hex(argon2id(key))` — a one-way, memory-hard hash, so it's a
//! safe *read* capability to put in a viewer URL: you can render the session from
//! the id but cannot recover the key (and thus cannot push) from it.

use argon2::Argon2;
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;

/// One font the client serves to viewers, uploaded in the register message (the
/// `/push` WebSocket's first message). The hub stores it per session and serves the
/// bytes at `/s/<id>/fonts/<key>`, which the page's `@font-face` references — so a
/// viewer renders the glyphs even without the font installed. Per-session storage
/// means two clients' fonts never clash.
#[derive(Serialize, Deserialize)]
pub struct FontAsset {
    /// URL-safe id, unique within the session (the font's index).
    pub key: String,
    pub mime: String,
    /// base64 of the font file bytes.
    pub b64: String,
}

/// Register message: the first message on the `/push` WebSocket — the page CSS, the
/// viewer template, and the fonts the CSS references. The client renders locally, so
/// it owns the template too and pushes it here; the hub just fills it per request.
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

/// Header carrying the secret key on the `/push` WebSocket upgrade.
pub const KEY_HEADER: &str = "x-shellglass-key";

/// Fixed application salt. The id must be a pure function of the secret (client
/// and hub derive it independently and must agree), so the salt can't be random
/// or per-hash — it's a constant. Memory-hardness, not the salt, is what slows a
/// brute force here.
///
/// The version suffix does double duty: it versions the derivation scheme AND
/// guards against protocol skew. Bump it on a breaking change to the **wire
/// messages** ([`crate::diff`]) a mismatched pair could both speak yet interpret
/// differently — a version-skewed client/hub then disagrees on `key → id`, so the
/// stale side fails loudly at the `/push` upgrade (403, "register its session id")
/// instead of streaming frames the other side silently drops. (A change that adds or
/// renames an *endpoint* needs no bump: an old client hits a route that's gone — a
/// loud 404, not a silent misread. Likewise a purely *additive* optional key — e.g.
/// the full frame's `i` inline-image list — needs no bump: an old decoder ignores
/// it and the text mirror stays correct, just without the new overlay; bump only
/// when an existing message would be *misread*.) The cost of a bump is that
/// operators re-run `print-id` and update `--allow` + view URLs; keys stay valid.
const SALT: &[u8] = b"shellglass/session-id/v4";

/// The management-API identity domain. Same derivation as [`session_id`],
/// DIFFERENT salt: domain separation. A leaked session key can never
/// authenticate to the hub's management API, and an API key can never push
/// frames — the two credential spaces cannot collide even for the same
/// underlying secret. Versioned independently of [`SALT`] (an API-breaking
/// change bumps this one alone; session pairs are unaffected).
const API_SALT: &[u8] = b"shellglass/api-id/v1";

/// Underivable id for a secret key in the given salt domain: Argon2id
/// (memory- and compute-hard) rendered as lowercase hex. Memory-hardness
/// makes brute-forcing a weak secret from the public id expensive; with a
/// strong random secret it's belt-and-suspenders. Hex (never `-`) so the id
/// is safe as a CLI value and URL path. Cost is paid once per connection or
/// API request, never per frame.
fn derive_id(key: &str, salt: &[u8]) -> String {
    let mut out = [0u8; 32];
    Argon2::default()
        .hash_password_into(key.as_bytes(), salt, &mut out)
        .expect("argon2id with a fixed valid salt and output length cannot fail");
    out.iter().fold(String::with_capacity(64), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// The session id for a push key — the read capability in view URLs and the
/// hub's `--allow` entries.
pub fn session_id(key: &str) -> String {
    derive_id(key, SALT)
}

/// The API id for a management key — the hub's `--api-allow` entries. See
/// [`API_SALT`] for why this is its own domain.
pub fn api_id(key: &str) -> String {
    derive_id(key, API_SALT)
}

/// Upper bound on a single WebSocket message the hub will accept from a pusher.
/// Sized for the **register** message — the first one, which carries every exported
/// font base64-encoded in one JSON blob (a heavy/CJK bundle is tens of MB, +33% for
/// base64).
/// Every later message is a [`crate::diff`] wire message (a rendered screen, far
/// smaller). Guards against a client making the hub buffer an unbounded message; the
/// client checks its register against this before connecting so an over-limit bundle
/// fails with a clear error instead of an endless reconnect.
pub const MAX_WS_MESSAGE: usize = 64 * 1024 * 1024;

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

    // Domain separation: the same secret yields UNRELATED ids in the session
    // and API domains — a leaked session key is not an API credential and
    // vice versa.
    #[test]
    fn api_domain_is_separate() {
        let key = "correct horse battery staple";
        let api = api_id(key);
        assert_eq!(api, api_id(key), "same key -> same api id");
        assert_ne!(api, session_id(key), "domains must not collide");
        assert_eq!(api.len(), 64);
        assert!(
            api.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
            "api id must be lowercase hex: {api}"
        );
    }
}

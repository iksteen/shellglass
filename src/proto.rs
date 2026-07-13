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
/// `/push` WebSocket's first message). The hub stores fonts in a hub-wide
/// content-addressed cache and serves them at `/s/<slug>/fonts/<key>`, where
/// the key is [`content_key`] — a hash the HUB computes from the pushed bytes.
/// There is deliberately NO client-chosen key on the wire: the client bakes
/// its own [`content_key`] into its CSS, and an honest client's URLs match the
/// hub's derivation; a lying client can only break its own font references,
/// never overwrite or poison a cache entry another session shares.
#[derive(Serialize, Deserialize)]
pub struct FontAsset {
    pub mime: String,
    /// base64 of the font file bytes.
    pub b64: String,
}

/// Content address of a served asset (fonts AND inline images):
/// `hex(sha256(mime · 0x00 · bytes))`. Both sides compute it independently —
/// the client to reference the asset (`fonts/<key>` in its `@font-face` CSS,
/// `images/<key>` in frame placements), the hub to store/serve the pushed
/// bytes. There is never a client-claimed key on the wire, so a client can
/// only break its own references, not overwrite an entry; the mime is part of
/// the hash so right bytes can't be pre-seeded under a wrong content type.
/// Content-addressed URLs are immutable, so responses cache forever.
pub fn content_key(mime: &str, bytes: &[u8]) -> String {
    use sha2::Digest as _;
    let mut h = sha2::Sha256::new();
    h.update(mime.as_bytes());
    h.update([0u8]);
    h.update(bytes);
    h.finalize()
        .iter()
        .fold(String::with_capacity(64), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// Register message: the first message on the `/push` WebSocket — the page CSS, the
/// viewer template, and the fonts the CSS references. The client renders locally, so
/// it owns the template too and pushes it here; the hub just fills it per request.
#[derive(Serialize, Deserialize)]
pub struct RegisterBody {
    pub css: String,
    /// Just the `@font-face` rules from `css`, served on its own at `style.css`
    /// so an iframe-less embed can `<link>` the web fonts without pulling the
    /// page-scoped base rules (which would leak onto a light-DOM host). Additive
    /// and `default` (empty): an older client omits it, and the hub falls back to
    /// serving the full `css` — so no protocol-skew (SALT) bump.
    #[serde(default)]
    pub font_css: String,
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

/// Client→hub upload of one inline-image payload, sent on the `/push`
/// WebSocket BEFORE any frame whose placements reference its content key —
/// WS ordering then guarantees the hub can serve the bytes by the time any
/// viewer sees the reference. Never forwarded to viewers (the hub intercepts
/// it by its distinctive `{"blob":` prefix). Like fonts, the wire carries no
/// client-chosen key: the hub derives [`content_key`] from the decoded bytes
/// itself, so a lying client can only break its own placements. Re-sent per
/// connection (the hub may have restarted); the per-session store dedups.
#[derive(Serialize, Deserialize)]
pub struct BlobMsg {
    pub blob: BlobBody,
}

#[derive(Serialize, Deserialize)]
pub struct BlobBody {
    /// MIME type (part of the content key).
    pub m: String,
    /// base64 of the image file bytes.
    pub d: String,
}

/// Header carrying the secret key on the `/push` WebSocket upgrade.
pub const KEY_HEADER: &str = "x-shellglass-key";

/// Fixed application salt. The id must be a pure function of the secret (client
/// and hub derive it independently and must agree), so the salt can't be random
/// or per-hash — it's a constant. Memory-hardness, not the salt, is what slows a
/// brute force here.
///
/// The version suffix versions the id-DERIVATION scheme only. Historically it
/// ALSO doubled as the wire-protocol-skew guard — a breaking change to the wire
/// messages ([`crate::diff`]) bumped it, which moved every session id, so a
/// stale client/hub disagreed on `key → id` and failed loudly (403) at the
/// upgrade instead of silently dropping frames. That worked but rotated every
/// operator's ids on what was often a cosmetic wire tweak.
///
/// **As of v6 that job has moved to explicit negotiation** ([`PROTOCOL_VERSION`]):
/// the client sends the wire protocol it speaks in the [`PROTOCOL_HEADER`], and a
/// hub that can't serve it answers `426` with its version + supported range (see
/// [`HUB_PROTOCOL_MIN`]). So **do NOT bump this salt on a wire-message change** —
/// bump [`PROTOCOL_VERSION`] instead, and no ids move. Bump SALT *only* if the id
/// derivation itself changes (a different KDF or input mapping); that rotates ids
/// (operators re-run `print-id`, update `--allow` + view URLs; keys stay valid).
/// v6 is intended to be the **last** such rotation.
///
/// History: v5 changed the register contract (pushed CSS carries page-RELATIVE
/// font URLs, stored verbatim). v6 was the final wire-coupled rotation — it landed
/// alongside negotiation and the fractional inline-image `w`/`h`; every wire change
/// after it is gated by [`PROTOCOL_VERSION`], not by moving this salt.
const SALT: &[u8] = b"shellglass/session-id/v6";

/// The wire-protocol version this build speaks. The push client sends it in the
/// [`PROTOCOL_HEADER`] at the `/push` upgrade; the hub serves the inclusive range
/// `[HUB_PROTOCOL_MIN, PROTOCOL_VERSION]` and rejects anything else with `426` +
/// version headers (so the operator learns which side to upgrade), instead of the
/// old scheme where a wire bump rotated every session id via [`SALT`].
///
/// **Bump this on a breaking change to the wire messages** ([`crate::diff`]) — a
/// change a mismatched pair could both speak yet interpret differently. A purely
/// *additive* optional key (an old decoder ignores it, mirror stays correct) needs
/// no bump, same as before; bump only when an existing message would be *misread*.
/// Protocol 1 is the baseline shipped with negotiation (it includes the fractional
/// image `w`/`h`). When bumping, also decide [`HUB_PROTOCOL_MIN`].
pub const PROTOCOL_VERSION: u32 = 1;

/// The OLDEST wire protocol a hub of this build still serves. The hub accepts a
/// client protocol in `[HUB_PROTOCOL_MIN, PROTOCOL_VERSION]`. Keep it below
/// [`PROTOCOL_VERSION`] while the hub retains backward-compatible handling for an
/// older wire; raise it (to drop that support) to reject old clients with a clear
/// "update the push client" message rather than misreading their frames.
pub const HUB_PROTOCOL_MIN: u32 = 1;

/// A build must serve the protocol it speaks, else it would 426 its own clients.
const _: () = assert!(
    HUB_PROTOCOL_MIN <= PROTOCOL_VERSION,
    "HUB_PROTOCOL_MIN must not exceed PROTOCOL_VERSION"
);

/// Request header carrying [`PROTOCOL_VERSION`] on the `/push` upgrade, alongside
/// [`KEY_HEADER`]. Absent ⇒ the hub assumes protocol 1 (a client old enough to
/// omit it predates negotiation but, on a matching id, still speaks the baseline).
pub const PROTOCOL_HEADER: &str = "x-shellglass-protocol";

/// Response headers on a `426` protocol rejection: the hub's exact version and the
/// inclusive protocol range it serves, so the client can tell the operator which
/// side to upgrade and to what minimum. The client VALIDATES the version before
/// echoing it (control-char-free by construction) — a `426` body/headers are still
/// peer-supplied.
pub const HUB_VERSION_HEADER: &str = "x-shellglass-hub-version";
pub const PROTOCOL_MIN_HEADER: &str = "x-shellglass-protocol-min";
pub const PROTOCOL_MAX_HEADER: &str = "x-shellglass-protocol-max";

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
///
/// `ext` is the optional per-system salt extension (`--id-salt`): appended to
/// the domain salt as `<domain>/<ext>`, it makes the same secret yield
/// different ids on differently-salted systems — de-amortizing precomputed
/// dictionaries over weak keys and unlinking a reused key across hubs. An
/// EMPTY extension appends nothing, so ids derived before the option existed
/// are byte-identical. Like the domain constants, the extension is a
/// must-match parameter of the id ecosystem, not a secret; changing a hub's
/// extension invalidates every registered id (a deliberate revocation lever,
/// same blast radius as a salt version bump).
fn derive_id(key: &str, salt: &[u8], ext: &str) -> String {
    let salted;
    let salt = if ext.is_empty() {
        salt
    } else {
        salted = [salt, b"/", ext.as_bytes()].concat();
        &salted
    };
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
/// hub's `--allow` entries. No salt extension; see [`session_id_ext`].
pub fn session_id(key: &str) -> String {
    session_id_ext(key, "")
}

/// [`session_id`] with a per-system salt extension (see [`derive_id`]).
pub fn session_id_ext(key: &str, ext: &str) -> String {
    derive_id(key, SALT, ext)
}

/// The API id for a management key — the hub's `--api-allow` entries. See
/// [`API_SALT`] for why this is its own domain. No salt extension; see
/// [`api_id_ext`].
pub fn api_id(key: &str) -> String {
    api_id_ext(key, "")
}

/// [`api_id`] with a per-system salt extension (see [`derive_id`]).
pub fn api_id_ext(key: &str, ext: &str) -> String {
    derive_id(key, API_SALT, ext)
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

/// Make an untrusted hub-supplied string safe to print to the operator's terminal.
/// The hub (or a MITM) could embed terminal control sequences — including via JSON
/// unicode escapes, decoded by the time this sees a `&str` — to inject into the
/// terminal. Unlike a transport error (whose *kind* is the signal, so the text is
/// dropped), some strings ARE content the operator wants (an API error body, the
/// hub's version on a 426), so we neuter rather than discard: strip control
/// characters and bound the length so a giant string can't flood the screen. Shared
/// by the `sessions` CLI ([`crate::apictl`]) and the push client's protocol-mismatch
/// message ([`crate::client`]).
pub fn neuter(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).take(256).collect()
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

    // The per-system salt extension: "" must be byte-identical to the
    // un-extended derivation (existing deployments keep their ids), a set
    // extension must diverge in both domains, and domain separation must
    // hold under the same extension.
    #[test]
    fn id_salt_extension_extends_both_domains() {
        let key = "correct horse battery staple";
        assert_eq!(
            session_id_ext(key, ""),
            session_id(key),
            "empty ext = stable ids"
        );
        assert_eq!(
            api_id_ext(key, ""),
            api_id(key),
            "empty ext = stable api ids"
        );
        let (s, a) = (session_id_ext(key, "hub-a"), api_id_ext(key, "hub-a"));
        assert_ne!(s, session_id(key), "ext diverges session ids");
        assert_ne!(a, api_id(key), "ext diverges api ids");
        assert_ne!(s, a, "domains stay separate under one ext");
        assert_ne!(
            s,
            session_id_ext(key, "hub-b"),
            "different systems, different ids"
        );
        assert_eq!(s.len(), 64, "same shape as un-extended ids");
    }

    #[test]
    fn neuter_strips_control_and_bounds() {
        assert_eq!(neuter("0.21.0"), "0.21.0", "ordinary text untouched");
        // A CSI clear-screen: no escape sequence survives to the terminal.
        let out = neuter("\x1b[2J\x1b[1;1Hgotcha");
        assert!(
            !out.chars().any(char::is_control),
            "no controls survive: {out:?}"
        );
        assert_eq!(out, "[2J[1;1Hgotcha");
        assert!(
            neuter(&"x".repeat(10_000)).chars().count() <= 256,
            "bounded so a giant string can't flood the screen"
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

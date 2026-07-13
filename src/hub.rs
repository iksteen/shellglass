//! Multi-session hub server: receives pushes from clients and serves viewers.
//!
//! It renders nothing and re-diffs nothing. A client opens one WebSocket to `/push`
//! (keyed by `session_id(secret)`, authorized once at the upgrade) and runs a tiny
//! state machine over it: the **first** message is a [`proto::RegisterBody`] (page
//! CSS + render config + fonts), every message **after** is a pre-encoded wire
//! message (a full picture, then deltas). The hub applies each wire message to the
//! session's full matrix ([`diff::Live::publish_wire`], so late-joining viewers get
//! a correct snapshot) and forwards the bytes to viewers verbatim. Viewers open
//! `/s/<slug>` and stream from `/s/<slug>/events`, where `<slug>` is the public view
//! handle an operator aliased the session to (`--allow <id>:<slug>`), defaulting to
//! the session id itself when no alias is given (see [`AllowConfig`]). The slug is the
//! read capability and the *only* way to view a session; the session id is the push
//! capability (never a view route on its own), and the secret behind it — never sent
//! to viewers — is the write capability.
//!
//! One WebSocket carries the whole push: one auth, no length-framing layer, and —
//! with a client ping/pong heartbeat and a SIGTERM Close — prompt detection of a
//! dead or restarting hub.

use crate::diff;
use crate::fonts::CACHE_CONTROL_IMMUTABLE;
use crate::model::Frame;
use crate::proto::{self, KEY_HEADER};
use crate::render;
use anyhow::{Context, Result, bail};
use axum::Router;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade, close_code};
use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::header::{ACCESS_CONTROL_ALLOW_ORIGIN, CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderName, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::sync::{Semaphore, broadcast};
use tower_http::compression::CompressionLayer;
// ponytail: pinned to axum's tungstenite (0.29) so the downcast below matches the
// concrete error axum boxes. On an axum WebSocket-stack bump, move this in lockstep —
// a mismatched major makes the downcast miss and the 1009 classification quietly
// falls back to a plain drop (graceful, but no "message too big" signal).
use tungstenite::Error as WsError;
use tungstenite::error::CapacityError;

struct Session {
    css: String,
    /// Just the `@font-face` rules (pushed separately), served at `style.css` for
    /// iframe-less embeds. Empty for an older client — the route then falls back
    /// to the full `css`.
    font_css: String,
    /// Viewer template the client pushed (empty → the hub's built-in default).
    template: String,
    /// Render config the client pushed (colors + symbol_map) for its `viewer.js`.
    render_cfg: String,
    /// Live publisher: decoded pushed frames in, per-viewer cell deltas out.
    live: Arc<diff::Live>,
    /// Content addresses ([`proto::content_key`]) of the fonts this session's
    /// pushed CSS references. The bytes live in the hub-wide [`FontCache`];
    /// this set is both the refcount stake AND the serving authorization —
    /// `/s/<slug>/fonts/<key>` only serves keys in the session's own set, so
    /// sessions stay isolated even though storage is shared.
    fonts: std::collections::HashSet<String>,
    /// Content-addressed inline-image payloads this session's pusher uploaded
    /// (blob messages), serving `/s/<slug>/images/<key>`. Per session — image
    /// content is session-specific, so unlike fonts there is nothing to share;
    /// dies with the session. Keys the current frame references are protected
    /// from the byte-cap eviction (see [`diff::ImageStore`]).
    images: Arc<Mutex<diff::ImageStore>>,
    /// Per-session kick: the management API's DELETE fires this so the live
    /// `/push` WebSocket Closes immediately (the pusher's next reconnect then
    /// 403s — its id is gone from the registry).
    kick: broadcast::Sender<()>,
    /// False for a registry STUB: the session is allowed but its pusher has
    /// never registered. The view route serves the built-in placeholder
    /// (default template + default CSS, no fonts, operator-offline) instead
    /// of pushed content; the first register flips it.
    registered: bool,
}

/// One session's status for the management API listing (`GET /api/sessions`).
pub struct SessionStatus {
    pub id: String,
    pub slug: String,
    /// An operator is currently pushing frames for this session.
    pub live: bool,
    /// Live viewer count per transport, as `(name, count)` — one entry per
    /// [`diff::Transport`], so a new transport flows through without touching this.
    pub viewers: Vec<(&'static str, usize)>,
}

/// Hub-wide content-addressed font store: one copy per distinct font, however
/// many sessions push it. The key is [`proto::content_key`], computed by the HUB
/// from the pushed bytes — there is no client-claimed key anywhere, so a
/// malicious client cannot overwrite or poison an entry other sessions share
/// (lying about the mime doesn't help either: it's part of the hash). `refs`
/// counts referencing sessions; an entry is evicted the moment it hits zero.
type FontCache = HashMap<String, FontEntry>;

struct FontEntry {
    mime: String,
    /// Cheaply cloneable for serving (one allocation, shared).
    bytes: bytes::Bytes,
    /// Number of sessions whose `fonts` set contains this key.
    refs: usize,
}

/// The offline-stub frame: a blank 80×24 screen with a red `shellglass: <msg>`
/// line at the top. An ordinary [`Frame::Screen`], NOT special-cased anywhere —
/// the text renders as cells (safe by construction, no HTML), the web viewer's
/// operator-offline overlay covers it, and the SSH view (cells-only) shows the
/// line. Replaces the removed `Frame::Banner`.
fn waiting_frame(msg: &str) -> Frame {
    let line: Vec<crate::model::StyledCell> = format!("shellglass: {msg}")
        .chars()
        .map(|ch| crate::model::StyledCell {
            text: ch.to_string(),
            fg: crate::model::Color::Idx(9), // bright red
            ..Default::default()
        })
        .collect();
    let mut rows = vec![line];
    rows.resize_with(24, Vec::new);
    Frame::Screen(crate::model::Grid {
        cols: 80,
        rows,
        cursor: None,
        cursor_style: 0,
        default_colors: (crate::model::Color::Default, crate::model::Color::Default),
        title: String::new(),
        links: std::collections::BTreeMap::new(),
        images: Vec::new(),
        image_data: HashMap::new(),
    })
}

/// Apply a session's font-set change to the cache: `new` gains stakes (using
/// `incoming` bytes for first insertion), `old` releases them, zero-ref
/// entries evicted. Call with BOTH the sessions lock and the cache lock held
/// (sessions → cache order), so set membership and refcounts can't skew.
fn retarget_fonts(
    cache: &mut FontCache,
    old: &std::collections::HashSet<String>,
    new: &std::collections::HashSet<String>,
    mut incoming: HashMap<String, (String, bytes::Bytes)>,
) {
    for key in new.difference(old) {
        if let Some(e) = cache.get_mut(key) {
            e.refs += 1;
        } else if let Some((mime, bytes)) = incoming.remove(key) {
            cache.insert(
                key.clone(),
                FontEntry {
                    mime,
                    bytes,
                    refs: 1,
                },
            );
        }
    }
    for key in old.difference(new) {
        if let Some(e) = cache.get_mut(key) {
            e.refs -= 1;
            if e.refs == 0 {
                cache.remove(key);
            }
        }
    }
}

/// Per-session byte cap for uploaded image payloads ([`diff::ImageStore`]).
/// Generous: on-screen images are protected from eviction regardless, so the
/// cap only bounds how much RECENTLY-OFF-SCREEN grace a session keeps.
const IMAGE_STORE_CAP: usize = 64 * 1024 * 1024;

/// Cap on concurrent `session_id` (argon2id) hashes. The hash is memory-hard
/// (~19 MiB, deliberately expensive) — unbounded concurrent grinding on bad keys
/// would exhaust memory and pin CPU, so authorize takes a permit before hashing.
/// Legitimate operators are a handful of allowlisted pushers reconnecting rarely,
/// so a small cap never contends; a flood just waits (and gets fail2ban'd). ponytail:
/// flat cap — raise it if legit operators ever queue behind each other.
const HASH_SLOTS: usize = 4;

/// Parsed `--allow` config: which session ids may push, and the public slug each
/// maps to in the view URL.
///
/// The **session id** (`session_id(secret)`) is the push capability, screened at the
/// `/push` upgrade. The **slug** is the *only* public view handle: viewers reach a
/// session at `/s/<slug>`, never at `/s/<session_id>`. An operator sets it with
/// `--allow <id>:<slug>`; with no `:slug` the slug defaults to the id itself, so an
/// un-aliased session is still viewed at `/s/<id>` exactly as before. Parsing rejects
/// a duplicate id, a duplicate slug, a malformed id, or a non-URL-safe slug up front
/// (see [`parse_allow`]).
#[derive(Default)]
pub struct AllowConfig {
    /// session_id → slug. Push auth checks membership by id; registration logs the slug.
    by_id: HashMap<String, String>,
    /// slug → session_id: the view namespace, holding only slugs (a plain lookup on the
    /// viewer hot path, no hashing). An un-aliased session's slug is its own id.
    by_view: HashMap<String, String>,
}

impl AllowConfig {
    /// True when no id is allowed — the hub would reject every push (`403`).
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }
}

/// Parse `--allow` entries (`<session_id>` or `<session_id>:<slug>`) into an
/// [`AllowConfig`], validating that every session id is well-formed hex, every slug is
/// URL-safe, and no two entries collide on either the push key (session id) or the view
/// key (slug). A collision or a malformed value is a hard startup error naming the
/// offending value — not a silently dropped duplicate.
pub fn parse_allow(entries: &[String]) -> Result<AllowConfig> {
    let mut cfg = AllowConfig::default();
    for entry in entries {
        // Split on the first ':' — id before, slug after; no ':' means "slug = id".
        let (id, slug) = match entry.split_once(':') {
            Some((id, slug)) => (id, slug),
            None => (entry.as_str(), entry.as_str()),
        };
        validate_id(id).with_context(|| format!("--allow entry {entry:?}"))?;
        validate_slug(slug).with_context(|| format!("--allow entry {entry:?}"))?;
        if cfg.by_id.contains_key(id) {
            bail!("--allow lists session id {id} more than once");
        }
        if let Some(other) = cfg.by_view.get(slug) {
            bail!("--allow slug {slug:?} is claimed by two sessions ({other} and {id})");
        }
        cfg.by_view.insert(slug.to_string(), id.to_string());
        cfg.by_id.insert(id.to_string(), slug.to_string());
    }
    Ok(cfg)
}

/// The session registry's on-disk form (`--sessions-file`): public ids and
/// slugs only — no secrets, no session content. Versioned envelope so a
/// future shape change fails loudly instead of misreading.
#[derive(serde::Serialize, serde::Deserialize)]
struct PersistFile {
    version: u32,
    sessions: Vec<PersistEntry>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct PersistEntry {
    id: String,
    slug: String,
}

/// Load the persisted registry. `Ok(None)` = the file doesn't exist yet
/// (first boot: the caller seeds from `--allow` and writes it). A file that
/// exists but can't be read/parsed/validated is a HARD error, deliberately
/// not a fallback to `--allow` — re-seeding over a corrupt store could
/// resurrect sessions the API deleted.
pub fn load_sessions(path: &std::path::Path) -> Result<Option<AllowConfig>> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading sessions file {path:?}")),
    };
    let file: PersistFile =
        serde_json::from_str(&text).with_context(|| format!("parsing sessions file {path:?}"))?;
    if file.version != 1 {
        bail!(
            "sessions file {path:?} has unsupported version {}",
            file.version
        );
    }
    let mut cfg = AllowConfig::default();
    for e in &file.sessions {
        validate_id(&e.id).with_context(|| format!("sessions file {path:?}"))?;
        validate_slug(&e.slug).with_context(|| format!("sessions file {path:?}"))?;
        if cfg.by_id.contains_key(&e.id) || cfg.by_view.contains_key(&e.slug) {
            bail!(
                "sessions file {path:?} has a duplicate id or slug ({})",
                e.id
            );
        }
        cfg.by_view.insert(e.slug.clone(), e.id.clone());
        cfg.by_id.insert(e.id.clone(), e.slug.clone());
    }
    Ok(Some(cfg))
}

/// Parse `--api-allow` entries (API ids from `print-id --api`) into the set
/// [`HubState::with_api_allowed`] takes. Ids have the same 64-hex shape as
/// session ids (same derivation, different salt domain); duplicates are a
/// startup error like `--allow`'s.
pub fn parse_api_allow(entries: &[String]) -> Result<std::collections::HashSet<String>> {
    let mut set = std::collections::HashSet::new();
    for id in entries {
        validate_id(id).with_context(|| format!("--api-allow entry {id:?}"))?;
        if !set.insert(id.clone()) {
            bail!("--api-allow lists api id {id} more than once");
        }
    }
    Ok(set)
}

/// A session id must be exactly what [`session_id`] emits — 64 lowercase hex chars.
/// Checking it here turns a fat-fingered id, or a `slug:id` written the wrong way
/// round, into a clear startup error instead of a session that can never be pushed to.
fn validate_id(id: &str) -> Result<()> {
    if id.len() != 64
        || !id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        bail!("session id must be 64 lowercase hex chars (from `print-id`), got {id:?}");
    }
    Ok(())
}

/// A slug is a URL path segment, so restrict it to unreserved URL characters
/// (`[A-Za-z0-9._~-]`) and forbid empty — keeping view URLs unambiguous and copy-safe.
fn validate_slug(slug: &str) -> Result<()> {
    if slug.is_empty() {
        bail!("slug must not be empty (use `--allow <id>` for no alias)");
    }
    if !slug
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~'))
    {
        bail!("slug {slug:?} must be URL-safe: only letters, digits, and -._~");
    }
    Ok(())
}

#[derive(Clone)]
pub struct HubState {
    sessions: Arc<Mutex<HashMap<String, Session>>>,
    /// Shared content-addressed font store (see [`FontCache`]). Lock order:
    /// `sessions` before `fonts` wherever both are held.
    fonts: Arc<Mutex<FontCache>>,
    /// The session registry: which ids (`session_id(secret)`) may push and the
    /// public view slug each maps to. Seeded from `--allow`, mutable at runtime
    /// through the management API — every lookup takes a short read lock; the
    /// only writers are the API's add/remove handlers. The operator/API adds
    /// ids, never secrets — the hub screens by hash.
    registry: Arc<std::sync::RwLock<AllowConfig>>,
    /// API ids (`api_id(secret)`, the API salt domain) permitted to call the
    /// management API. CLI-configured (`--api-allow`), not runtime-mutable.
    /// Empty = the API is off and the whole `/api` namespace 404s.
    api_allowed: Arc<std::collections::HashSet<String>>,
    /// Where the registry persists (`--sessions-file`), if the operator opted
    /// in. `None` = memory-only: runtime changes die with the process and
    /// `--allow` re-seeds every start.
    persist_path: Option<Arc<std::path::PathBuf>>,
    /// Serializes `persist()` so two concurrent management-API mutations can't
    /// interleave into the shared `.tmp` path (a torn write renamed into place
    /// hard-errors on next startup). Held across snapshot+write+rename, so the
    /// last mutation's snapshot is what lands on disk.
    persist_lock: Arc<Mutex<()>>,
    /// Public base URL (`scheme://host:port`, no trailing slash) for logging the
    /// view URL when a new session connects.
    base: Arc<str>,
    /// Per-system salt extension (`--id-salt`) applied when hashing pushed
    /// keys and API keys — must match what the operator's `gen-key`/`print-id`
    /// used. Empty = the un-extended derivation.
    id_salt: Arc<str>,
    /// Permits gating concurrent argon2 hashes (see [`HASH_SLOTS`]).
    hash_slots: Arc<Semaphore>,
    /// Fires once on SIGTERM: each open `/push` WebSocket sends a Close and returns
    /// so pushers detect the shutdown immediately (see `main`'s graceful path).
    shutdown: broadcast::Sender<()>,
}

impl HubState {
    pub fn new(allow: AllowConfig, base: String) -> Self {
        let (shutdown, _) = broadcast::channel(1);
        let st = Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            fonts: Arc::new(Mutex::new(FontCache::new())),
            registry: Arc::new(std::sync::RwLock::new(allow)),
            api_allowed: Arc::new(std::collections::HashSet::new()),
            persist_path: None,
            persist_lock: Arc::new(Mutex::new(())),
            base: base.into(),
            id_salt: "".into(),
            hash_slots: Arc::new(Semaphore::new(HASH_SLOTS)),
            shutdown,
        };
        // Every seeded --allow entry gets its placeholder immediately: the
        // view URL works (operator-offline) before any pusher connects.
        let seeded: Vec<String> = st.registry.read().unwrap().by_id.keys().cloned().collect();
        for id in seeded {
            st.ensure_stub(&id);
        }
        st
    }

    // (waiting_frame is a free fn below.)

    /// Make sure `id` has at least a STUB session, so `/s/<slug>` and its SSE
    /// stream exist from the moment the session is registered (CLI or API):
    /// an offline blank screen with a "waiting for operator…" line — the
    /// viewer's `operator` event machinery shows the same offline state as a
    /// live session whose pusher dropped. A no-op when the session already exists.
    fn ensure_stub(&self, id: &str) {
        let mut map = self.sessions.lock().unwrap();
        if map.contains_key(id) {
            return;
        }
        let live = diff::Live::new(Arc::new(waiting_frame("waiting for operator…")));
        live.set_online(false);
        let (kick, _) = broadcast::channel(1);
        map.insert(
            id.to_string(),
            Session {
                css: String::new(),
                font_css: String::new(),
                template: String::new(),
                render_cfg: String::new(),
                live,
                fonts: std::collections::HashSet::new(),
                images: Arc::new(Mutex::new(diff::ImageStore::new(IMAGE_STORE_CAP))),
                kick,
                registered: false,
            },
        );
    }

    /// Enable the management API for these API ids (`--api-allow`). Builder
    /// style; call before the state is cloned into the router.
    #[must_use]
    pub fn with_api_allowed(mut self, ids: impl IntoIterator<Item = String>) -> Self {
        self.api_allowed = Arc::new(ids.into_iter().collect());
        self
    }

    /// Set the per-system salt extension (`--id-salt`) used when hashing
    /// pushed/API keys. Builder style; call before the state is cloned.
    #[must_use]
    pub fn with_id_salt(mut self, ext: String) -> Self {
        self.id_salt = ext.into();
        self
    }

    /// Persist the registry to `path` on every mutation (`--sessions-file`).
    /// Builder style; call before the state is cloned into the router.
    #[must_use]
    pub fn with_persistence(mut self, path: std::path::PathBuf) -> Self {
        self.persist_path = Some(Arc::new(path));
        self
    }

    /// Write the registry to the sessions file (atomic: temp + rename). A
    /// no-op without `--sessions-file`. Entries are sorted for a stable,
    /// diffable file.
    ///
    /// # Errors
    /// Filesystem failures; the in-memory registry is authoritative either
    /// way — the API surfaces the error so the operator knows disk diverged.
    pub fn persist(&self) -> Result<()> {
        let Some(path) = &self.persist_path else {
            return Ok(());
        };
        // Serialize the whole snapshot→write→rename so concurrent mutations
        // can't clobber the shared temp path or land a stale snapshot last.
        let _guard = self.persist_lock.lock().unwrap();
        let mut sessions: Vec<PersistEntry> = {
            let reg = self.registry.read().unwrap();
            reg.by_id
                .iter()
                .map(|(id, slug)| PersistEntry {
                    id: id.clone(),
                    slug: slug.clone(),
                })
                .collect()
        };
        sessions.sort_by(|a, b| a.slug.cmp(&b.slug));
        let file = PersistFile {
            version: 1,
            sessions,
        };
        let text = serde_json::to_string_pretty(&file).context("serializing the registry")?;
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("creating sessions-file directory {dir:?}"))?;
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, text).with_context(|| format!("writing {tmp:?}"))?;
        std::fs::rename(&tmp, path.as_ref())
            .with_context(|| format!("moving {tmp:?} into place"))?;
        Ok(())
    }

    /// The public slug for an allowed session id; `None` = not (or no longer)
    /// registered.
    fn slug_of(&self, id: &str) -> Option<String> {
        self.registry.read().unwrap().by_id.get(id).cloned()
    }

    /// The session id a view slug resolves to; `None` = unknown slug.
    fn id_of_view(&self, slug: &str) -> Option<String> {
        self.registry.read().unwrap().by_view.get(slug).cloned()
    }

    /// Whether `id` may push (the `/push` authorization check).
    fn is_allowed(&self, id: &str) -> bool {
        self.registry.read().unwrap().by_id.contains_key(id)
    }

    /// Register a session at runtime (the management API's POST). Same
    /// validation and uniqueness rules as `--allow`, as results instead of
    /// startup errors.
    pub fn add_session(&self, id: &str, slug: Option<&str>) -> Result<(), AddError> {
        validate_id(id).map_err(|e| AddError::Invalid(e.to_string()))?;
        let slug = slug.unwrap_or(id);
        validate_slug(slug).map_err(|e| AddError::Invalid(e.to_string()))?;
        let mut reg = self.registry.write().unwrap();
        if reg.by_id.contains_key(id) {
            return Err(AddError::IdTaken);
        }
        if reg.by_view.contains_key(slug) {
            return Err(AddError::SlugTaken);
        }
        reg.by_view.insert(slug.to_string(), id.to_string());
        reg.by_id.insert(id.to_string(), slug.to_string());
        drop(reg);
        // The view URL works immediately, operator-offline, before any push.
        self.ensure_stub(id);
        Ok(())
    }

    /// Remove a session BY ITS SESSION ID (the management API is explicit
    /// about the two namespaces — see `remove_by_slug`). Returns false when
    /// the id isn't registered.
    pub fn remove_by_id(&self, id: &str) -> bool {
        let removed = {
            let mut reg = self.registry.write().unwrap();
            match reg.by_id.remove(id) {
                Some(slug) => {
                    reg.by_view.remove(&slug);
                    true
                }
                None => false,
            }
        };
        if removed {
            self.drop_session_state(id);
        }
        removed
    }

    /// Remove a session BY ITS VIEW SLUG. Returns the removed session's id,
    /// `None` when the slug isn't registered. A separate method (and API
    /// route) from `remove_by_id` by design: an un-aliased slug IS the id,
    /// so one ambiguous lookup could target the wrong namespace.
    pub fn remove_by_slug(&self, slug: &str) -> Option<String> {
        let id = {
            let mut reg = self.registry.write().unwrap();
            let id = reg.by_view.remove(slug)?;
            reg.by_id.remove(&id);
            id
        };
        self.drop_session_state(&id);
        Some(id)
    }

    /// Every registered session for the management API's reconciliation listing.
    /// `live` = an operator is currently pushing; `web`/`ssh` = live viewer counts
    /// per transport. A registry STUB (allowed but never registered) has no `Session`
    /// yet, so it reports `live=false` with zero viewers.
    pub fn list_sessions(&self) -> Vec<SessionStatus> {
        let reg = self.registry.read().unwrap();
        let map = self.sessions.lock().unwrap();
        reg.by_id
            .iter()
            .map(|(id, slug)| {
                let s = map.get(id);
                SessionStatus {
                    id: id.clone(),
                    slug: slug.clone(),
                    live: s.is_some_and(|s| s.live.is_online()),
                    viewers: diff::Transport::ALL
                        .iter()
                        .map(|&t| (t.name(), s.map_or(0, |s| s.live.viewer_count(t))))
                        .collect(),
                }
            })
            .collect()
    }

    /// Drop a removed session's stored state (CSS/fonts/render-config/matrix)
    /// and kick its live pusher, if any. Viewer SSE streams end when the
    /// `Live` drops with them; the session's font stakes are released (a font
    /// no other session references is evicted from the shared cache).
    fn drop_session_state(&self, id: &str) {
        let mut map = self.sessions.lock().unwrap();
        if let Some(s) = map.remove(id) {
            let mut cache = self.fonts.lock().unwrap();
            retarget_fonts(
                &mut cache,
                &s.fonts,
                &std::collections::HashSet::new(),
                HashMap::new(),
            );
            drop(cache);
            drop(map);
            let _ = s.kick.send(());
        }
    }

    /// The `Live` publisher for a public view slug, if a client has registered the
    /// session it names. Used by the SSH viewer to resolve `ssh <slug>@hub` to the
    /// session's frames (an un-aliased session's slug is its own id, so
    /// `ssh <id>@hub` still works).
    pub(crate) fn live(&self, slug: &str) -> Option<Arc<diff::Live>> {
        let id = self.id_of_view(slug)?;
        let map = self.sessions.lock().unwrap();
        map.get(&id).map(|s| Arc::clone(&s.live))
    }

    /// Signal every open `/push` WebSocket to close (graceful shutdown). Called from
    /// `main`'s SIGTERM handler; a no-op if no pushers are connected.
    pub fn trigger_shutdown(&self) {
        let _ = self.shutdown.send(());
    }
}

/// Why a runtime session registration was refused — the management API maps
/// these onto 409 (taken) and 400 (invalid).
#[derive(Debug, PartialEq, Eq)]
pub enum AddError {
    /// The session id is already registered.
    IdTaken,
    /// The slug is claimed by another session.
    SlugTaken,
    /// Malformed id or non-URL-safe slug (the message names the rule).
    Invalid(String),
}

/// Resolve a request's key to its (allowed) session id, or the status to reject
/// with: `401` if no key, `403` if the key isn't pre-registered on the hub.
///
/// The key is hashed once with argon2id (deliberately expensive, memory-hard). Two
/// DoS guards wrap that: a [`HASH_SLOTS`] permit caps how many hashes run at once
/// (bounds peak memory + CPU under a bad-key flood), and the hash runs on the
/// blocking pool so it never starves the async workers serving viewers. Every
/// rejection also logs a parseable line for fail2ban (see [`log_reject`]) so an
/// operator can ban a persistent grinder.
async fn authorize(
    st: &HubState,
    headers: &HeaderMap,
    peer: SocketAddr,
    route: &str,
) -> Result<String, StatusCode> {
    // No key ⇒ no hash: reject cheaply without spending a permit or a hash.
    let Some(key) = key_of(headers) else {
        log_reject(headers, peer, route, StatusCode::UNAUTHORIZED);
        return Err(StatusCode::UNAUTHORIZED);
    };
    // Hold a permit across the hash only; released before the handler streams. The
    // semaphore is never closed, so acquire can't error.
    let id = {
        let _permit = st.hash_slots.acquire().await.expect("hash_slots open");
        let ext = Arc::clone(&st.id_salt);
        tokio::task::spawn_blocking(move || proto::session_id_ext(&key, &ext))
            .await
            .expect("hash task")
    };
    if st.is_allowed(&id) {
        Ok(id)
    } else {
        log_reject(headers, peer, route, StatusCode::FORBIDDEN);
        Err(StatusCode::FORBIDDEN)
    }
}

/// Parseable auth-failure line for fail2ban. `client` is the effective client IP
/// (first `X-Forwarded-For` hop if present, else the socket peer); `peer` is always
/// the raw TCP source so a directly-exposed hub can ban on it — XFF is
/// attacker-controlled unless a trusted proxy sets it.
fn log_reject(headers: &HeaderMap, peer: SocketAddr, route: &str, code: StatusCode) {
    let client = header_str(headers, "x-forwarded-for")
        .and_then(|v| v.split(',').next())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map_or_else(|| peer.ip().to_string(), str::to_string);
    // "auth failure" (not "push auth failure"): the same line now covers the
    // management API's rejections too; the route names which surface.
    eprintln!(
        "shellglass: auth failure {} on {route} client={client} peer={}",
        code.as_u16(),
        peer.ip()
    );
}

/// Resolve a management-API request's `Authorization: Bearer <key>` to an
/// authorization decision, or the status to reject with: `404` while the API
/// is unconfigured (the whole namespace stays hidden), `401` with no usable
/// header, `403` when the key's API id isn't on `--api-allow`. The key hashes
/// in the API salt domain ([`proto::api_id`]) — a session key can never pass
/// here — behind the same argon2 DoS guards and fail2ban-parseable rejection
/// logging as [`authorize`].
async fn authorize_api(
    st: &HubState,
    headers: &HeaderMap,
    peer: SocketAddr,
    route: &'static str,
) -> Result<(), StatusCode> {
    if st.api_allowed.is_empty() {
        return Err(StatusCode::NOT_FOUND);
    }
    let key = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            v.strip_prefix("Bearer ")
                .or_else(|| v.strip_prefix("bearer "))
        })
        .map(str::to_string);
    let Some(key) = key else {
        log_reject(headers, peer, route, StatusCode::UNAUTHORIZED);
        return Err(StatusCode::UNAUTHORIZED);
    };
    let id = {
        let _permit = st.hash_slots.acquire().await.expect("hash_slots open");
        let ext = Arc::clone(&st.id_salt);
        tokio::task::spawn_blocking(move || proto::api_id_ext(&key, &ext))
            .await
            .expect("hash task")
    };
    if st.api_allowed.contains(&id) {
        Ok(())
    } else {
        log_reject(headers, peer, route, StatusCode::FORBIDDEN);
        Err(StatusCode::FORBIDDEN)
    }
}

/// Whether a WebSocket recv error is a message that exceeded the size limit — i.e.
/// answer it with a 1009 "Message Too Big" Close. axum boxes the raw tungstenite
/// error, so this matches on the `Capacity(MessageTooLong)` variant rather than its
/// text. A version mismatch (see the `tungstenite` import note) just makes the
/// downcast miss and returns false — a graceful fall back to a plain drop.
fn is_message_too_long(err: &(dyn std::error::Error + Send + Sync + 'static)) -> bool {
    matches!(
        err.downcast_ref::<WsError>(),
        Some(WsError::Capacity(CapacityError::MessageTooLong { .. }))
    )
}

pub fn app(state: HubState) -> Router {
    app_with_cors(state, &[])
}

/// `cors_origins`: exact origins (or a single `*`) allowed to read the per-session
/// data routes an iframe-less embed fetches cross-origin. Empty (default) =
/// same-origin only (the nginx-same-origin embed needs nothing here). `/embed.js`
/// keeps its own unconditional ACAO `*` and stays outside this layer.
pub fn app_with_cors(state: HubState, cors_origins: &[String]) -> Router {
    // Compress the page + fonts, but never the SSE stream (compression buffers and
    // would defeat the realtime push). So layer per-route, not globally.
    let compress = CompressionLayer::new();
    // The routes an iframe-less cross-origin embed fetches — per session, plus the
    // root viewer.js. Grouped so the configurable CORS layer lands only here.
    let mut data = Router::new()
        .route("/viewer.js", get(viewer_js).layer(compress.clone()))
        .route("/s/{slug}/events", get(events))
        .route("/s/{slug}/snapshot", get(snapshot).layer(compress.clone()))
        .route("/s/{slug}/config", get(config).layer(compress.clone()))
        .route(
            "/s/{slug}/style.css",
            get(style_css).layer(compress.clone()),
        )
        .route(
            "/s/{slug}/viewer.js",
            get(viewer_js).layer(compress.clone()),
        )
        .route(
            "/s/{slug}/favicon.svg",
            get(favicon).layer(compress.clone()),
        )
        .route("/s/{slug}/fonts/{key}", get(font).layer(compress.clone()))
        // No compression: image formats are already compressed.
        .route("/s/{slug}/images/{key}", get(image));
    if let Some(cors) = crate::server_cors(cors_origins) {
        data = data.layer(cors);
    }
    Router::new()
        .route("/", get(index))
        // The push client's single WebSocket: register-then-stream state machine,
        // authorized once at the upgrade.
        .route("/push", get(ws_push))
        .route("/embed.js", get(embed_js).layer(compress.clone()))
        .route("/favicon.svg", get(favicon).layer(compress))
        // Views are canonical at /s/<slug>/ (trailing slash): the page's URLs
        // are all RELATIVE (events, fonts/<i>, viewer.js, favicon.svg — routed
        // per slug below), so pages survive a subpath-mounting reverse proxy
        // that neither the hub nor the client can know about. The slash-less
        // form redirects RELATIVELY, which a prefixing proxy also survives.
        .route("/s/{slug}", get(view_redirect))
        .route("/s/{slug}/", get(view))
        // The management API (Bearer-authorized in the API salt domain; the
        // namespace 404s while --api-allow is unconfigured). Delete is two
        // explicit routes: an un-aliased slug IS the id, so one ambiguous
        // route could target the wrong namespace.
        .route("/api/sessions", get(api_list).post(api_add))
        .route(
            "/api/sessions/by-id/{id}",
            axum::routing::delete(api_delete_by_id),
        )
        .route(
            "/api/sessions/by-slug/{slug}",
            axum::routing::delete(api_delete_by_slug),
        )
        .merge(data)
        .with_state(state)
}

/// POST body for `/api/sessions`: the session's public id (from `print-id`,
/// never a key) and an optional view slug (defaults to the id, exactly like
/// `--allow <id>` without `:slug`).
#[derive(serde::Deserialize)]
struct ApiAddBody {
    id: String,
    #[serde(default)]
    slug: Option<String>,
}

fn api_json(code: StatusCode, body: &serde_json::Value) -> Response {
    (code, [(CONTENT_TYPE, "application/json")], body.to_string()).into_response()
}

/// Persist after a successful mutation. The in-memory registry is
/// authoritative and stays mutated either way; a write failure surfaces as a
/// 500 so the managing tool knows disk diverged (the next successful
/// mutation re-writes the full registry and heals it).
fn persist_after(st: &HubState) -> Option<Response> {
    match st.persist() {
        Ok(()) => None,
        Err(e) => {
            eprintln!("shellglass: sessions-file write failed: {e:#}");
            Some(api_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                &serde_json::json!({
                    "error": format!("applied in memory, but persisting failed: {e:#}")
                }),
            ))
        }
    }
}

/// `POST /api/sessions` — register a session at runtime. The body is taken
/// raw and parsed AFTER authorization, so an unauthorized caller learns
/// nothing (not even that its JSON was malformed) and the unconfigured
/// namespace stays a plain 404.
async fn api_add(
    State(st): State<HubState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: String,
) -> Response {
    if let Err(code) = authorize_api(&st, &headers, peer, "/api/sessions").await {
        return code.into_response();
    }
    let body: ApiAddBody = match serde_json::from_str(&body) {
        Ok(b) => b,
        Err(e) => {
            return api_json(
                StatusCode::BAD_REQUEST,
                &serde_json::json!({ "error": format!("invalid JSON body: {e}") }),
            );
        }
    };
    match st.add_session(&body.id, body.slug.as_deref()) {
        Ok(()) => {
            if let Some(err) = persist_after(&st) {
                return err;
            }
            let slug = body.slug.unwrap_or_else(|| body.id.clone());
            println!("shellglass: api added session {} (slug {slug})", body.id);
            api_json(
                StatusCode::CREATED,
                &serde_json::json!({ "id": body.id, "slug": slug }),
            )
        }
        Err(AddError::IdTaken) => api_json(
            StatusCode::CONFLICT,
            &serde_json::json!({ "error": "session id already registered" }),
        ),
        Err(AddError::SlugTaken) => api_json(
            StatusCode::CONFLICT,
            &serde_json::json!({ "error": "slug already in use" }),
        ),
        Err(AddError::Invalid(m)) => {
            api_json(StatusCode::BAD_REQUEST, &serde_json::json!({ "error": m }))
        }
    }
}

/// `DELETE /api/sessions/by-id/{id}` — remove a session BY ITS SESSION ID.
async fn api_delete_by_id(
    State(st): State<HubState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(code) = authorize_api(&st, &headers, peer, "/api/sessions/by-id").await {
        return code.into_response();
    }
    if st.remove_by_id(&id) {
        if let Some(err) = persist_after(&st) {
            return err;
        }
        println!("shellglass: api removed session {id} (by id)");
        StatusCode::NO_CONTENT.into_response()
    } else {
        api_json(
            StatusCode::NOT_FOUND,
            &serde_json::json!({ "error": "unknown session id" }),
        )
    }
}

/// `DELETE /api/sessions/by-slug/{slug}` — remove a session BY ITS VIEW SLUG.
async fn api_delete_by_slug(
    State(st): State<HubState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(slug): Path<String>,
) -> Response {
    if let Err(code) = authorize_api(&st, &headers, peer, "/api/sessions/by-slug").await {
        return code.into_response();
    }
    match st.remove_by_slug(&slug) {
        Some(id) => {
            if let Some(err) = persist_after(&st) {
                return err;
            }
            println!("shellglass: api removed session {id} (by slug {slug})");
            StatusCode::NO_CONTENT.into_response()
        }
        None => api_json(
            StatusCode::NOT_FOUND,
            &serde_json::json!({ "error": "unknown slug" }),
        ),
    }
}

/// `GET /api/sessions` — every registered session, for reconciliation.
async fn api_list(
    State(st): State<HubState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    if let Err(code) = authorize_api(&st, &headers, peer, "/api/sessions").await {
        return code.into_response();
    }
    let sessions: Vec<serde_json::Value> = st
        .list_sessions()
        .into_iter()
        .map(|s| {
            let mut obj = serde_json::Map::new();
            obj.insert("id".into(), s.id.into());
            obj.insert("slug".into(), s.slug.into());
            obj.insert("live".into(), s.live.into());
            // One `<name>Viewers` key per transport (e.g. `webViewers`, `sshViewers`).
            for (name, count) in s.viewers {
                obj.insert(format!("{name}Viewers"), count.into());
            }
            serde_json::Value::Object(obj)
        })
        .collect();
    api_json(StatusCode::OK, &serde_json::Value::Array(sessions))
}

fn key_of(headers: &HeaderMap) -> Option<String> {
    headers.get(KEY_HEADER)?.to_str().ok().map(str::to_string)
}

/// Public base URL for logging a view link, honoring reverse-proxy headers so the
/// URL matches the address a viewer actually reaches (e.g. behind Traefik). Takes
/// scheme from `X-Forwarded-Proto`, host from `X-Forwarded-Host` then `Host`, and
/// a mount prefix from `X-Forwarded-Prefix` (set by prefix-stripping proxies —
/// the pages themselves are prefix-agnostic via relative URLs, but a LOGGED link
/// must spell the prefix out); falls back to the configured base for whichever
/// part is absent. XFF headers are comma-lists (proxy chain) — the first token is
/// the original client-facing value.
fn view_base(headers: &HeaderMap, configured: &str) -> String {
    let fwd = |name| {
        header_str(headers, name)
            .and_then(|v| v.split(',').next())
            .map(str::trim)
    };
    let (def_scheme, def_host) = configured
        .split_once("://")
        .map_or(("http", configured), |(s, h)| (s, h));
    let scheme = match fwd("x-forwarded-proto")
        .filter(|s| !s.is_empty())
        .unwrap_or(def_scheme)
    {
        // These headers ride the /push WebSocket upgrade, where proxies report
        // the WS scheme — but the view link is a plain HTTP(S) page.
        "wss" => "https",
        "ws" => "http",
        s => s,
    };
    let host = fwd("x-forwarded-host")
        .or_else(|| header_str(headers, "host"))
        .filter(|s| !s.is_empty())
        .unwrap_or(def_host);
    let prefix = fwd("x-forwarded-prefix")
        .map(|p| p.trim_end_matches('/'))
        .filter(|p| !p.is_empty())
        .unwrap_or("");
    format!("{scheme}://{host}{prefix}")
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name)?.to_str().ok()
}

/// The push client's WebSocket. Authorize once at the upgrade (moving the argon2
/// semaphore + fail2ban guards here — one hash per connection, not one per
/// register *and* one per stream), then run the register-then-stream state machine.
/// A `426 Upgrade Required` telling an authorized pusher this hub can't serve its
/// wire protocol. The headers are the machine-readable contract the push client
/// renders into an actionable message (this hub's exact version + the inclusive
/// protocol range it serves); the body is a plain-text fallback for a human hitting
/// `/push` directly. Replaces the old id-rotation 403 for wire skew.
fn protocol_mismatch(client_proto: u32) -> Response {
    let ver = env!("CARGO_PKG_VERSION");
    let body = format!(
        "shellglass hub {ver}: serves push protocol {min}-{max}; client requested {client_proto}.\n\
         Update the {side} so their protocol versions overlap.\n",
        min = proto::HUB_PROTOCOL_MIN,
        max = proto::PROTOCOL_VERSION,
        side = if client_proto > proto::PROTOCOL_VERSION {
            "hub"
        } else {
            "push client"
        },
    );
    (
        StatusCode::UPGRADE_REQUIRED,
        [
            (
                HeaderName::from_static(proto::HUB_VERSION_HEADER),
                ver.to_string(),
            ),
            (
                HeaderName::from_static(proto::PROTOCOL_MIN_HEADER),
                proto::HUB_PROTOCOL_MIN.to_string(),
            ),
            (
                HeaderName::from_static(proto::PROTOCOL_MAX_HEADER),
                proto::PROTOCOL_VERSION.to_string(),
            ),
        ],
        body,
    )
        .into_response()
}

async fn ws_push(
    State(st): State<HubState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let id = match authorize(&st, &headers, peer, "/push").await {
        Ok(id) => id,
        Err(code) => return code.into_response(),
    };
    // Wire-protocol negotiation, AFTER auth (only a valid key learns the hub's
    // version). SALT is frozen now, so a legit client authorizes even across
    // protocol changes — this check, not an id rotation, is what catches a wire
    // skew, and answers with an actionable 426 (which side to upgrade, to what
    // minimum) instead of a misleading 403. Absent header ⇒ protocol 1 (baseline).
    let client_proto = headers
        .get(proto::PROTOCOL_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(1);
    if !(proto::HUB_PROTOCOL_MIN..=proto::PROTOCOL_VERSION).contains(&client_proto) {
        eprintln!(
            "shellglass: push {id} rejected: protocol {client_proto} outside supported {}-{}",
            proto::HUB_PROTOCOL_MIN,
            proto::PROTOCOL_VERSION
        );
        return protocol_mismatch(client_proto);
    }
    // The view URL to announce on first registration — computed now, while we still
    // have the upgrade request's proxy headers.
    let base = view_base(&headers, &st.base);
    // Cap both the message and the *frame*: tungstenite sends a message as one
    // unfragmented frame, so the frame limit (16 MiB by default) would otherwise
    // reject a 16–64 MiB register before the message limit ever applied. A frame
    // over the cap is rejected at its header — the body is never buffered.
    ws.max_message_size(proto::MAX_WS_MESSAGE)
        .max_frame_size(proto::MAX_WS_MESSAGE)
        .on_upgrade(move |socket| push_session(st, id, base, socket))
}

/// Drive one push connection: the first Text is the [`proto::RegisterBody`] (creates
/// or refreshes the session), every Text after is a wire message applied to the
/// session's matrix + forwarded to viewers. Ends on Close, a socket error, or the
/// shutdown signal (sends a Close so the pusher reconnects promptly). On exit the
/// session + its last frame are **kept** so viewers still see the frozen screen.
async fn push_session(st: HubState, id: String, base: String, mut socket: WebSocket) {
    let mut shutdown = st.shutdown.subscribe();
    // None until the register message arrives; the state machine is "have we a Live".
    let mut live: Option<Arc<diff::Live>> = None;
    // The session's kick channel (management-API delete), armed at register.
    let mut kick: Option<broadcast::Receiver<()>> = None;
    loop {
        tokio::select! {
            _ = shutdown.recv() => {
                let _ = socket.send(Message::Close(None)).await;
                break;
            }
            _ = async {
                match kick.as_mut() {
                    Some(k) => { let _ = k.recv().await; }
                    None => std::future::pending::<()>().await,
                }
            } => {
                // Deleted by the management API: Close so the pusher notices at
                // once; its reconnect then 403s (the id is gone). The session
                // state was already dropped — don't touch the orphaned Live.
                eprintln!("shellglass: push {id} removed by the management API — closing");
                let _ = socket.send(Message::Close(None)).await;
                live = None;
                break;
            }
            msg = socket.recv() => {
                let msg = match msg {
                    Some(Ok(m)) => m,
                    // A recv error — a message over MAX_WS_MESSAGE, a protocol
                    // violation, or an abrupt drop. (Our own client refuses to send an
                    // oversized register; this handles other or buggy clients.)
                    Some(Err(e)) => {
                        let phase = if live.is_none() { "register" } else { "stream" };
                        // Match the size case on the error *variant* (not its text):
                        // axum boxes the raw tungstenite error, so downcast and check
                        // for Capacity(MessageTooLong). If so, answer precisely with a
                        // 1009 "Message Too Big" Close + reason, so a client sees an
                        // actionable error instead of a bare drop it treats as a
                        // transient blip and retries forever. The frame-size limit
                        // rejected it at the header, so the hub never buffered the body.
                        // Best-effort: a client mid-send of a huge message isn't
                        // reading and may only observe the drop.
                        let inner = e.into_inner();
                        if is_message_too_long(&*inner) {
                            let mib = proto::MAX_WS_MESSAGE / (1024 * 1024);
                            let _ = socket
                                .send(Message::Close(Some(CloseFrame {
                                    code: close_code::SIZE,
                                    reason: format!("message exceeds the {mib} MiB limit").into(),
                                })))
                                .await;
                            eprintln!("shellglass: push {id} sent an over-limit {phase} message ({inner})");
                        } else {
                            eprintln!("shellglass: push {id} dropped during {phase}: {inner}");
                        }
                        break;
                    }
                    None => break, // clean close
                };
                match msg {
                    Message::Text(t) => match &live {
                        // AwaitingRegister: the first message must parse as a
                        // RegisterBody; anything else is a protocol error → close.
                        None => match serde_json::from_str::<proto::RegisterBody>(t.as_str()) {
                            Ok(reg) => match register_session(&st, &id, &base, reg) {
                                Some((l, k)) => {
                                    live = Some(l);
                                    kick = Some(k);
                                }
                                // Deleted between the upgrade's authorize and this
                                // register — the API raced the connect; close.
                                None => {
                                    eprintln!(
                                        "shellglass: push {id} was removed before it registered — closing"
                                    );
                                    let _ = socket.send(Message::Close(None)).await;
                                    break;
                                }
                            },
                            Err(e) => {
                                eprintln!(
                                    "shellglass: push {id} sent an invalid register message ({e}) — closing"
                                );
                                let _ = socket.send(Message::Close(None)).await;
                                break;
                            }
                        },
                        // Streaming: image payload uploads are intercepted and
                        // stored (never forwarded — viewers fetch over HTTP);
                        // everything else is a wire message: apply + forward.
                        // publish_wire drops malformed or out-of-sync messages
                        // rather than the whole session.
                        Some(l) => {
                            if t.as_str().starts_with("{\"blob\":") {
                                store_blob(&st, &id, l, t.as_str());
                            } else {
                                l.publish_wire(t.as_str());
                            }
                        }
                    },
                    Message::Close(_) => break,
                    // Ping is auto-ponged by axum; Pong/Binary are ignored.
                    _ => {}
                }
            }
        }
    }
    // Pusher gone (drop, error, or shutdown): flag the operator offline so viewers
    // see the session is no longer live. The session + last frame are kept, so the
    // frozen screen stays up. `None` = died before registering; nothing to flag.
    // ponytail: last-writer-wins if two pushers share one id — the rarer one exiting
    // marks the session offline while the other still streams. Single-pusher is the
    // norm; add a refcount if concurrent pushers become real.
    if let Some(l) = &live {
        l.set_online(false);
    }
}

/// Store an uploaded image payload in the session's image store, keyed by the
/// hash the HUB derives from the decoded bytes — a client cannot choose the
/// key, so it can only ever provide the bytes its own frames reference (same
/// no-poisoning doctrine as fonts). Protection for the eviction pass comes
/// from the session's current applied frame: whatever is on screen right now
/// must stay servable. Malformed messages are dropped, like bad wire messages.
fn store_blob(st: &HubState, id: &str, live: &diff::Live, msg: &str) {
    let Ok(m) = serde_json::from_str::<proto::BlobMsg>(msg) else {
        return;
    };
    let Ok(bytes) = B64.decode(m.blob.d) else {
        return;
    };
    let hash = proto::content_key(&m.blob.m, &bytes);
    let Frame::Screen(g) = &*live.frame();
    let protected: std::collections::HashSet<String> =
        g.images.iter().map(|p| p.hash.clone()).collect();
    let store = {
        let map = st.sessions.lock().unwrap();
        match map.get(id) {
            Some(s) => Arc::clone(&s.images),
            None => return, // deleted mid-stream; the kick is on its way
        }
    };
    store
        .lock()
        .unwrap()
        .insert(hash, m.blob.m, bytes::Bytes::from(bytes), &protected);
}

/// Create or refresh the session for `id` from a register message; returns its
/// `Live` plus a receiver for the session's kick channel (fired when the
/// management API deletes the session). New sessions get a "waiting…" screen
/// (replaced by the first pushed frame) and announce their view URL once —
/// reconnects hit the refresh branch, so no spam. `None` = the session was
/// deleted between the upgrade's authorize and this register (the API raced
/// the connect); the caller closes.
fn register_session(
    st: &HubState,
    id: &str,
    base: &str,
    reg: proto::RegisterBody,
) -> Option<(Arc<diff::Live>, broadcast::Receiver<()>)> {
    // Decode uploaded fonts and derive OUR OWN content address for each;
    // silently drop any with bad base64 (the family just falls back in the
    // browser rather than failing the whole registration). The wire carries
    // no client-chosen key — the hash the hub computes here is the only name
    // a font ever has, so a client can only ever affect entries its own CSS
    // references; the shared cache cannot be overwritten or poisoned.
    let incoming: HashMap<String, (String, bytes::Bytes)> = reg
        .fonts
        .into_iter()
        .filter_map(|f| {
            let bytes = bytes::Bytes::from(B64.decode(f.b64).ok()?);
            Some((proto::content_key(&f.mime, &bytes), (f.mime, bytes)))
        })
        .collect();
    let keys: std::collections::HashSet<String> = incoming.keys().cloned().collect();
    // The id's public slug (for the announce log below). The pushed CSS is
    // stored VERBATIM: its font URLs are page-relative (`fonts/<content_key>`,
    // the SALT v5 register contract), which the canonical `/s/<slug>/` page
    // resolves for any slug and behind any subpath mount — nothing to rewrite.
    let slug = st.slug_of(id)?;
    let css = reg.css;
    let font_css = reg.font_css;
    let mut map = st.sessions.lock().unwrap();
    if let Some(s) = map.get_mut(id) {
        // The common path: every allowed id has at least a stub (ensure_stub),
        // so a first register is "stub becomes real" and a reconnect is a
        // refresh. Either way the pushed CSS/config/fonts replace what's
        // stored, and viewers sitting on the placeholder page reload through
        // its operator-online hook (see the view route).
        s.css = css;
        s.font_css = font_css;
        s.template = reg.template;
        s.render_cfg = reg.render_cfg;
        // Swap the session's font stakes: new fonts enter the shared cache,
        // ones only this session referenced are evicted.
        let mut cache = st.fonts.lock().unwrap();
        retarget_fonts(&mut cache, &s.fonts, &keys, incoming);
        drop(cache);
        s.fonts = keys;
        if !s.registered {
            s.registered = true;
            println!("shellglass: session connected — view at {base}/s/{slug}/");
        }
        // Coming (back) online — new stubs start offline, dropped pushers were
        // marked offline by push_session.
        s.live.set_online(true);
        Some((Arc::clone(&s.live), s.kick.subscribe()))
    } else {
        // Fallback only: an authorized id always has a stub, but keep the
        // create path for the theoretical gap.
        let live = diff::Live::new(Arc::new(waiting_frame("waiting for client…")));
        let (kick, kick_rx) = broadcast::channel(1);
        let mut cache = st.fonts.lock().unwrap();
        retarget_fonts(
            &mut cache,
            &std::collections::HashSet::new(),
            &keys,
            incoming,
        );
        drop(cache);
        map.insert(
            id.to_string(),
            Session {
                css,
                font_css,
                template: reg.template,
                render_cfg: reg.render_cfg,
                live: Arc::clone(&live),
                fonts: keys,
                images: Arc::new(Mutex::new(diff::ImageStore::new(IMAGE_STORE_CAP))),
                kick,
                registered: true,
            },
        );
        println!("shellglass: session connected — view at {base}/s/{slug}/");
        Some((live, kick_rx))
    }
}

/// Serve a font from the shared cache (the page's `@font-face` points here).
/// Public like `view`/`events` — the slug in the path is the read capability —
/// but only keys in the SESSION'S OWN reference set are served: storage is
/// hub-wide, the namespace stays per-session, so one session can't enumerate
/// or serve another's fonts through its slug.
async fn font(State(st): State<HubState>, Path((slug, key)): Path<(String, String)>) -> Response {
    let id = st.id_of_view(&slug);
    let referenced = {
        let map = st.sessions.lock().unwrap();
        match id.and_then(|id| map.get(&id)) {
            Some(s) => s.fonts.contains(&key),
            None => return (StatusCode::NOT_FOUND, "unknown session").into_response(),
        }
    };
    if !referenced {
        return (StatusCode::NOT_FOUND, "unknown font").into_response();
    }
    let entry = {
        let cache = st.fonts.lock().unwrap();
        cache.get(&key).map(|e| (e.mime.clone(), e.bytes.clone()))
    };
    match entry {
        // Bytes is refcounted — the clone is one Arc bump, not a copy.
        Some((mime, bytes)) => (
            [
                (CONTENT_TYPE, mime),
                (CACHE_CONTROL, CACHE_CONTROL_IMMUTABLE.to_string()),
            ],
            bytes,
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "unknown font").into_response(),
    }
}

/// Serve an uploaded inline-image payload from the session's own store (the
/// frame placements' `images/<key>` URLs point here). Public like the font
/// route — the slug is the read capability; the store being per-session is
/// the isolation. Content-addressed ⇒ immutable, cache forever.
async fn image(State(st): State<HubState>, Path((slug, key)): Path<(String, String)>) -> Response {
    let id = st.id_of_view(&slug);
    let store = {
        let map = st.sessions.lock().unwrap();
        match id.and_then(|id| map.get(&id)) {
            Some(s) => Arc::clone(&s.images),
            None => return (StatusCode::NOT_FOUND, "unknown session").into_response(),
        }
    };
    let entry = store.lock().unwrap().get(&key);
    match entry {
        Some((mime, bytes)) => (
            [
                (CONTENT_TYPE, mime),
                (CACHE_CONTROL, CACHE_CONTROL_IMMUTABLE.to_string()),
            ],
            bytes,
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "unknown image").into_response(),
    }
}

/// `/s/<slug>` → `/s/<slug>/`, the canonical directory-shaped page URL (the
/// page's asset/SSE URLs are relative). The Location is RELATIVE too, so the
/// redirect survives a subpath-mounting proxy; the query (`?embed`) rides
/// along.
async fn view_redirect(Path(slug): Path<String>, uri: axum::http::Uri) -> Response {
    let mut loc = format!("{slug}/");
    if let Some(q) = uri.query() {
        loc.push('?');
        loc.push_str(q);
    }
    (
        StatusCode::PERMANENT_REDIRECT,
        [(axum::http::header::LOCATION, loc)],
    )
        .into_response()
}

async fn view(
    State(st): State<HubState>,
    Path(slug): Path<String>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Response {
    // `?embed`: the chrome-less fit-to-frame page (what an <iframe> shows).
    // Always the built-in embed template — a pusher's custom template never
    // applies, so an embed's look is predictable for host pages; the pushed
    // CSS/fonts/render config still do (correctness, not chrome).
    let embed = params.contains_key("embed");
    let id = st.id_of_view(&slug);
    let map = st.sessions.lock().unwrap();
    let Some(s) = id.and_then(|id| map.get(&id)) else {
        return (StatusCode::NOT_FOUND, "unknown session").into_response();
    };
    if !s.registered {
        // Registered-but-unseeded: the pusher has never connected, so there is
        // no pushed CSS/template/fonts to serve. Serve the built-in template
        // with plain defaults (custom fonts deliberately ignored) — the SSE
        // stream connects to the stub's offline Live, so the page shows the
        // same operator-offline state as a live session whose pusher dropped.
        // The extra observer reloads the page the moment the operator comes
        // online: the reload then fetches the REAL page (pushed CSS + fonts),
        // which this placeholder cannot render.
        let script = format!(
            "{}\n<script>new MutationObserver(() => {{\n\
             const s = document.body.dataset.offline;\n\
             if (s === undefined || s === \"\") location.reload();\n\
             }}).observe(document.body, {{ attributes: true, attributeFilter: [\"data-offline\"] }});</script>",
            render::sse_script("events", render::DEFAULT_RENDER_CFG)
        );
        return (
            [(CACHE_CONTROL, "no-cache")],
            Html(render::page(
                if embed {
                    render::EMBED_TEMPLATE
                } else {
                    render::DEFAULT_TEMPLATE
                },
                &render::default_head_css(),
                &script,
            )),
        )
            .into_response();
    }
    let script = render::sse_script("events", &s.render_cfg);
    // Empty template = an older client that didn't push one; use the built-in.
    let template = if embed {
        render::EMBED_TEMPLATE
    } else if s.template.is_empty() {
        render::DEFAULT_TEMPLATE
    } else {
        &s.template
    };
    // Empty #screen: the renderer fills it from the first SSE frame (the hub
    // renders nothing itself). no-cache: the auto-reload path depends on a reload
    // fetching fresh HTML (fingerprinted /viewer.js?v=… URL + the version pair).
    (
        [(CACHE_CONTROL, "no-cache")],
        Html(render::page(template, &s.css, &script)),
    )
        .into_response()
}

async fn events(State(st): State<HubState>, Path(slug): Path<String>) -> Response {
    let Some(live) = st.live(&slug) else {
        return (StatusCode::NOT_FOUND, "unknown session").into_response();
    };
    live.connect()
}

/// `GET /s/<slug>/snapshot` — the session's current state as a one-shot JSON blob,
/// the same full-frame message a new SSE viewer receives first (no version hello, no
/// stream). For a consumer that wants a point-in-time read without holding `/events`
/// open; a stub session returns its operator-offline frame.
async fn snapshot(State(st): State<HubState>, Path(slug): Path<String>) -> Response {
    let Some(live) = st.live(&slug) else {
        return (StatusCode::NOT_FOUND, "unknown session").into_response();
    };
    (
        [(CONTENT_TYPE, "application/json")],
        live.snapshot().to_string(),
    )
        .into_response()
}

/// The iframe-less embed boot object (`window.SHELLGLASS` as JSON) for a session:
/// the client's relayed render config, or the built-in default for an unregistered
/// (operator-offline) stub. `events` is relative — the embed resolves it against
/// the session base.
async fn config(State(st): State<HubState>, Path(slug): Path<String>) -> Response {
    let id = st.id_of_view(&slug);
    let map = st.sessions.lock().unwrap();
    let Some(s) = id.and_then(|id| map.get(&id)) else {
        return (StatusCode::NOT_FOUND, "unknown session").into_response();
    };
    let cfg = if s.registered && !s.render_cfg.is_empty() {
        &s.render_cfg
    } else {
        render::DEFAULT_RENDER_CFG
    };
    (
        [
            (CONTENT_TYPE, "application/json"),
            (CACHE_CONTROL, "no-cache"),
        ],
        render::config_json("events", cfg),
    )
        .into_response()
}

/// The session's `@font-face` rules for an iframe-less embed to `<link>` (see the
/// standalone `style_css`). Pushed separately by the client; an older client that
/// didn't push `font_css` falls back to the full pushed CSS (the base rules then
/// ride along — harmless in a shadow-DOM embed, and a light-DOM embed can opt for
/// shadow if it matters). An unregistered stub has no fonts yet.
async fn style_css(State(st): State<HubState>, Path(slug): Path<String>) -> Response {
    let id = st.id_of_view(&slug);
    let map = st.sessions.lock().unwrap();
    let Some(s) = id.and_then(|id| map.get(&id)) else {
        return (StatusCode::NOT_FOUND, "unknown session").into_response();
    };
    let css = if !s.font_css.is_empty() {
        s.font_css.clone()
    } else {
        s.css.clone()
    };
    (
        [(CONTENT_TYPE, "text/css"), (CACHE_CONTROL, "no-cache")],
        css,
    )
        .into_response()
}

/// Serve the baked renderer (see [`crate::server`] for the caching rationale:
/// fingerprinted URL, immutable).
async fn viewer_js() -> Response {
    (
        [
            (CONTENT_TYPE, "application/javascript"),
            (CACHE_CONTROL, "public, max-age=31536000, immutable"),
        ],
        render::VIEWER_JS,
    )
        .into_response()
}

/// The stable embedding shim (see `render::EMBED_JS`). Cached for a day, not
/// immutable: host pages reference it un-fingerprinted, and it must be able
/// to pick up a fix within a deploy cycle. The documented snippet is a classic
/// script (no CORS involved); ACAO * keeps a `type="module"` load — which
/// fetches with CORS — working for hosts that prefer the element form.
async fn embed_js() -> Response {
    (
        [
            (CONTENT_TYPE, "application/javascript"),
            (CACHE_CONTROL, "public, max-age=86400"),
            (ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
        ],
        render::EMBED_JS,
    )
        .into_response()
}

async fn favicon() -> Response {
    (
        [
            (CONTENT_TYPE, "image/svg+xml"),
            (CACHE_CONTROL, "public, max-age=86400"),
        ],
        render::FAVICON_SVG,
    )
        .into_response()
}

async fn index() -> Html<&'static str> {
    Html("<p style=\"font-family:monospace\">shellglass hub — open /s/&lt;slug&gt;</p>")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::session_id;

    #[test]
    fn only_preregistered_keys_are_allowed() {
        let st = HubState::new(
            parse_allow(&[session_id("good-secret")]).unwrap(),
            String::new(),
        );
        assert!(
            st.is_allowed(&session_id("good-secret")),
            "registered key allowed"
        );
        assert!(
            !st.is_allowed(&session_id("other-secret")),
            "unregistered key rejected"
        );
        // An empty allowlist rejects everything (no implicit open hub).
        let empty = HubState::new(AllowConfig::default(), String::new());
        assert!(!empty.is_allowed(&session_id("good-secret")));
    }

    // The management API's runtime mutations: --allow semantics as results.
    #[test]
    fn runtime_add_and_remove() {
        let a = session_id("a");
        let b = session_id("b");
        let st = HubState::new(AllowConfig::default(), String::new());

        // add: un-aliased (slug = id) and aliased
        st.add_session(&a, None).unwrap();
        st.add_session(&b, Some("beta")).unwrap();
        assert!(st.is_allowed(&a) && st.is_allowed(&b));
        assert_eq!(st.id_of_view(&a).as_deref(), Some(a.as_str()));
        assert_eq!(st.id_of_view("beta").as_deref(), Some(b.as_str()));
        assert_eq!(st.id_of_view(&b), None, "aliased id is not a view route");

        // uniqueness rules, as results not panics
        assert_eq!(st.add_session(&a, None), Err(AddError::IdTaken));
        assert_eq!(
            st.add_session(&session_id("c"), Some("beta")),
            Err(AddError::SlugTaken)
        );
        assert!(matches!(
            st.add_session("not-hex", None),
            Err(AddError::Invalid(_))
        ));
        assert!(matches!(
            st.add_session(&session_id("c"), Some("bad slug")),
            Err(AddError::Invalid(_))
        ));

        // remove BY SLUG: resolves through the view namespace only
        assert_eq!(st.remove_by_slug("beta").as_deref(), Some(b.as_str()));
        assert!(!st.is_allowed(&b), "removed session may not push");
        assert_eq!(st.id_of_view("beta"), None);
        assert_eq!(st.remove_by_slug("beta"), None, "second delete: gone");

        // remove BY ID: works regardless of aliasing
        assert!(st.remove_by_id(&a));
        assert!(!st.is_allowed(&a));
        assert!(!st.remove_by_id(&a), "second delete: gone");

        // the two namespaces stay distinct: removing an ALIASED session by
        // its id-shaped SLUG string must not touch the id namespace
        let d = session_id("d");
        st.add_session(&d, Some("delta")).unwrap();
        assert_eq!(
            st.remove_by_slug(&d),
            None,
            "id is not a slug for an aliased session"
        );
        assert!(
            st.is_allowed(&d),
            "session survives the wrong-namespace call"
        );
    }

    // --allow seeds are "just another entry": the API deletes them exactly
    // like runtime-added ones (a restart re-seeds them — the flags are the
    // declarative baseline, the API the runtime layer).
    #[test]
    fn cli_seeded_sessions_are_api_deletable() {
        let a = session_id("a");
        let st = HubState::new(parse_allow(&[format!("{a}:alpha")]).unwrap(), String::new());
        assert!(st.live("alpha").is_some(), "seed gets its placeholder stub");
        assert_eq!(st.remove_by_slug("alpha").as_deref(), Some(a.as_str()));
        assert!(!st.is_allowed(&a), "seeded session removed like any other");
        assert!(st.live("alpha").is_none(), "stub gone with it");
    }

    /// A unique scratch path per test (same pid ⇒ name must differ per call site).
    fn scratch(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "shellglass-test-{}-{name}.json",
            std::process::id()
        ))
    }

    #[test]
    fn sessions_file_missing_means_seed() {
        assert!(
            load_sessions(&scratch("missing")).unwrap().is_none(),
            "no file yet → Ok(None), caller seeds from --allow"
        );
    }

    #[test]
    fn sessions_file_corrupt_is_a_hard_error() {
        let path = scratch("corrupt");
        std::fs::write(&path, "not json").unwrap();
        assert!(load_sessions(&path).is_err(), "garbage must not parse");
        std::fs::write(&path, r#"{"version":2,"sessions":[]}"#).unwrap();
        assert!(load_sessions(&path).is_err(), "unknown version rejected");
        let a = session_id("a");
        std::fs::write(
            &path,
            format!(
                r#"{{"version":1,"sessions":[{{"id":"{a}","slug":"x"}},{{"id":"{a}","slug":"y"}}]}}"#
            ),
        )
        .unwrap();
        assert!(load_sessions(&path).is_err(), "duplicate id rejected");
        std::fs::remove_file(&path).unwrap();
    }

    // The full persistence loop: mutate → file written → a fresh load matches
    // memory, including a deletion of an --allow-seeded session surviving.
    #[test]
    fn sessions_file_roundtrips_mutations() {
        let path = scratch("roundtrip");
        let _ = std::fs::remove_file(&path);
        let a = session_id("a");
        let b = session_id("b");
        let st = HubState::new(parse_allow(&[format!("{a}:alpha")]).unwrap(), String::new())
            .with_persistence(path.clone());
        st.persist().unwrap(); // the startup seed write

        st.add_session(&b, Some("beta")).unwrap();
        st.persist().unwrap();
        st.remove_by_slug("alpha").unwrap();
        st.persist().unwrap();

        let loaded = load_sessions(&path).unwrap().expect("file exists");
        assert_eq!(loaded.by_id.get(&b).map(String::as_str), Some("beta"));
        assert!(
            !loaded.by_id.contains_key(&a),
            "deleting a seeded session sticks across a reload"
        );
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn list_sessions_reports_registry() {
        let a = session_id("a");
        let st = HubState::new(AllowConfig::default(), String::new());
        st.add_session(&a, Some("alpha")).unwrap();
        let list = st.list_sessions();
        assert_eq!(list.len(), 1);
        let s = &list[0];
        assert_eq!(s.id, a);
        assert_eq!(s.slug, "alpha");
        assert!(!s.live, "no pusher has registered");
        assert_eq!(
            s.viewers,
            vec![("web", 0), ("ssh", 0)],
            "a stub has no viewers, one entry per transport"
        );
    }

    #[test]
    fn parse_allow_defaults_slug_to_id_and_aliases() {
        let a = session_id("a");
        let b = session_id("b");
        let cfg = parse_allow(&[format!("{a}:alpha"), b.clone()]).unwrap();
        // Aliased: the slug is the view handle; the raw id is NOT viewable.
        assert_eq!(cfg.by_view.get("alpha"), Some(&a));
        assert_eq!(
            cfg.by_view.get(&a),
            None,
            "an aliased id is not a view route"
        );
        assert_eq!(cfg.by_id.get(&a).map(String::as_str), Some("alpha"));
        // Un-aliased: the slug defaults to the id, so `/s/<id>` still resolves.
        assert_eq!(cfg.by_view.get(&b), Some(&b));
        assert_eq!(cfg.by_id.get(&b), Some(&b));
    }

    #[test]
    fn parse_allow_rejects_collisions() {
        let a = session_id("a");
        let b = session_id("b");
        // Duplicate session id.
        assert!(parse_allow(&[a.clone(), a.clone()]).is_err());
        assert!(parse_allow(&[format!("{a}:x"), format!("{a}:y")]).is_err());
        // Duplicate slug across two ids.
        assert!(parse_allow(&[format!("{a}:same"), format!("{b}:same")]).is_err());
        // One session's slug equal to another un-aliased session's id (= its slug).
        assert!(parse_allow(&[a.clone(), format!("{b}:{a}")]).is_err());
        // An id aliased to itself is idempotent, not a collision.
        assert!(parse_allow(&[format!("{a}:{a}")]).is_ok());
    }

    #[test]
    fn parse_allow_validates_id_and_slug_shape() {
        let a = session_id("a");
        assert!(parse_allow(&["not-hex".into()]).is_err(), "id not 64 hex");
        assert!(
            parse_allow(&[format!("{}:s", &a[..63])]).is_err(),
            "id too short"
        );
        assert!(parse_allow(&[format!("{a}:")]).is_err(), "empty slug");
        assert!(
            parse_allow(&[format!("{a}:bad/slug")]).is_err(),
            "slug has a '/'"
        );
        assert!(
            parse_allow(&[format!("{a}:ok.slug-1_2~3")]).is_ok(),
            "url-safe slug accepted"
        );
    }

    fn reg(css: &str) -> proto::RegisterBody {
        proto::RegisterBody {
            css: css.into(),
            font_css: String::new(),
            template: String::new(),
            render_cfg: String::new(),
            fonts: vec![],
        }
    }

    fn reg_with_fonts(css: &str, fonts: &[(&str, &[u8])]) -> proto::RegisterBody {
        use base64::Engine as _;
        proto::RegisterBody {
            css: css.into(),
            font_css: String::new(),
            template: String::new(),
            render_cfg: String::new(),
            fonts: fonts
                .iter()
                .map(|(mime, bytes)| proto::FontAsset {
                    mime: (*mime).to_string(),
                    b64: base64::engine::general_purpose::STANDARD.encode(bytes),
                })
                .collect(),
        }
    }

    // The shared font cache: content-addressed (hub-computed keys), one copy
    // per distinct font however many sessions push it, refcounted, evicted at
    // zero references, and served only through a session that references it.
    #[test]
    fn font_cache_dedups_refcounts_and_evicts() {
        let a = session_id("a");
        let b = session_id("b");
        let st = HubState::new(
            parse_allow(&[format!("{a}:one"), format!("{b}:two")]).unwrap(),
            "http://h".into(),
        );
        let shared: (&str, &[u8]) = ("font/ttf", b"SHARED-FONT-BYTES");
        let only_b: (&str, &[u8]) = ("font/ttf", b"B-ONLY-FONT");
        let shared_key = proto::content_key(shared.0, shared.1);

        register_session(&st, &a, "http://h", reg_with_fonts("x", &[shared])).unwrap();
        register_session(&st, &b, "http://h", reg_with_fonts("x", &[shared, only_b])).unwrap();
        {
            let cache = st.fonts.lock().unwrap();
            assert_eq!(cache.len(), 2, "same bytes stored once across sessions");
            assert_eq!(cache.get(&shared_key).unwrap().refs, 2);
        }

        // A re-register that drops the shared font releases ONE stake only.
        register_session(&st, &b, "http://h", reg_with_fonts("x", &[only_b])).unwrap();
        assert_eq!(
            st.fonts.lock().unwrap().get(&shared_key).unwrap().refs,
            1,
            "session A still holds its stake"
        );

        // Deleting the last referencing session evicts.
        st.remove_by_id(&a);
        st.drop_session_state(&a);
        assert!(
            !st.fonts.lock().unwrap().contains_key(&shared_key),
            "zero-ref font evicted"
        );
        st.remove_by_id(&b);
        st.drop_session_state(&b);
        assert!(st.fonts.lock().unwrap().is_empty(), "cache fully drained");
    }

    // Image blobs: stored under the HUB-derived content key (no client-chosen
    // key exists), served from the session's own store, isolated per session,
    // and dropped when malformed.
    #[tokio::test]
    async fn image_blobs_store_and_serve_per_session() {
        use base64::Engine as _;
        let a = session_id("a");
        let b = session_id("b");
        let st = HubState::new(
            parse_allow(&[format!("{a}:one"), format!("{b}:two")]).unwrap(),
            "http://h".into(),
        );
        let (live_a, _k) = register_session(&st, &a, "http://h", reg("x")).unwrap();
        register_session(&st, &b, "http://h", reg("x")).unwrap();

        let bytes = b"PNG-ISH-BYTES";
        let key = proto::content_key("image/png", bytes);
        let msg = format!(
            "{{\"blob\":{{\"m\":\"image/png\",\"d\":\"{}\"}}}}",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        );
        assert!(msg.starts_with("{\"blob\":"), "intercept prefix holds");
        store_blob(&st, &a, &live_a, &msg);
        // Malformed payloads are dropped, not fatal.
        store_blob(
            &st,
            &a,
            &live_a,
            "{\"blob\":{\"m\":\"x\",\"d\":\"@@notb64\"}}",
        );
        store_blob(&st, &a, &live_a, "{\"blob\":42}");

        let router = app(st);
        let res = router
            .clone()
            .oneshot(
                Request::get(format!("/s/one/images/{key}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK, "own image serves");
        assert!(
            res.headers()
                .get("cache-control")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.contains("immutable")),
            "content address caches forever"
        );
        let body = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        assert_eq!(&body[..], bytes);

        // Another session's slug must not serve it (per-session store).
        let res = router
            .clone()
            .oneshot(
                Request::get(format!("/s/two/images/{key}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    // Storage is shared; the serving NAMESPACE is not: a session's slug only
    // serves keys in that session's own reference set.
    #[tokio::test]
    async fn font_serving_is_isolated_per_session() {
        let a = session_id("a");
        let b = session_id("b");
        let st = HubState::new(
            parse_allow(&[format!("{a}:one"), format!("{b}:two")]).unwrap(),
            "http://h".into(),
        );
        let fa: (&str, &[u8]) = ("font/ttf", b"A-FONT");
        let key_a = proto::content_key(fa.0, fa.1);
        register_session(&st, &a, "http://h", reg_with_fonts("x", &[fa])).unwrap();
        register_session(&st, &b, "http://h", reg("x")).unwrap();
        let router = app(st);

        let res = router
            .clone()
            .oneshot(
                Request::get(format!("/s/one/fonts/{key_a}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK, "own font serves");
        let body = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        assert_eq!(&body[..], b"A-FONT");

        // Session B's slug must NOT serve A's font, even though the bytes sit
        // in the shared cache.
        let res = router
            .clone()
            .oneshot(
                Request::get(format!("/s/two/fonts/{key_a}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            res.status(),
            StatusCode::NOT_FOUND,
            "cross-session font access denied"
        );
    }

    #[test]
    fn message_too_long_is_classified_by_variant() {
        // Construct the exact error axum boxes for an over-limit frame, then confirm
        // the classifier keys on the variant (this is what triggers the 1009 Close).
        let err = axum::Error::new(WsError::Capacity(CapacityError::MessageTooLong {
            size: 100 * 1024 * 1024,
            max_size: proto::MAX_WS_MESSAGE,
        }));
        assert!(
            is_message_too_long(&*err.into_inner()),
            "MessageTooLong → 1009"
        );

        // An unrelated WS error is not a size rejection, so it must NOT send 1009.
        let other = axum::Error::new(WsError::AlreadyClosed);
        assert!(
            !is_message_too_long(&*other.into_inner()),
            "a non-capacity error must fall through to a plain drop"
        );
    }

    #[test]
    fn register_creates_then_reconnect_reuses_the_live() {
        let id = session_id("secret");
        let st = HubState::new(
            parse_allow(std::slice::from_ref(&id)).unwrap(),
            "http://h".into(),
        );
        // Seeding creates a STUB immediately: the view URL and its SSE stream
        // exist before any pusher, operator-offline.
        let stub = st.live(&id).expect("stub Live exists before any register");
        assert!(!stub.is_online(), "stub starts operator-offline");

        // First register (the WS's first message) adopts the stub's Live —
        // placeholder viewers already subscribed aren't orphaned.
        let (live1, _kick1) = register_session(&st, &id, "http://h", reg("a{}")).unwrap();
        assert!(
            Arc::ptr_eq(&stub, &live1),
            "register must adopt the stub's Live"
        );
        assert!(live1.is_online(), "register brings the operator online");

        // A reconnect re-registers: the CSS refreshes but the same Live is reused, so
        // viewers already subscribed don't get orphaned.
        let (live2, _kick2) = register_session(&st, &id, "http://h", reg("b{}")).unwrap();
        assert!(
            Arc::ptr_eq(&live1, &live2),
            "reconnect must reuse the session's Live, not replace it"
        );
        assert_eq!(
            st.sessions.lock().unwrap().get(&id).unwrap().css,
            "b{}",
            "re-register refreshes the pushed CSS"
        );
    }

    #[tokio::test]
    async fn snapshot_serves_the_current_full_frame() {
        let id = session_id("secret");
        let st = HubState::new(
            parse_allow(&[format!("{id}:one")]).unwrap(),
            "http://h".into(),
        );
        let (live, _kick) = register_session(&st, &id, "http://h", reg("a{}")).unwrap();
        let want = live.snapshot().to_string();

        let router = app(st);
        let res = router
            .clone()
            .oneshot(Request::get("/s/one/snapshot").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            res.headers()
                .get(CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("application/json"),
        );
        let body = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        assert_eq!(
            &body[..],
            want.as_bytes(),
            "serves the live snapshot verbatim"
        );
        // A valid full-frame message (carries the `d` block list) — directly usable.
        let v: serde_json::Value = serde_json::from_slice(&body).expect("valid JSON");
        assert!(v.get("d").is_some(), "full-frame shape: {v}");

        // An unknown slug 404s, like /events.
        let res = router
            .oneshot(
                Request::get("/s/nope/snapshot")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn register_stores_css_verbatim() {
        let id = session_id("secret");
        let st = HubState::new(
            parse_allow(&[format!("{id}:pretty")]).unwrap(),
            "http://h".into(),
        );
        // The SALT v5 register contract: font URLs arrive page-relative
        // (`fonts/<i>`), which the canonical `/s/<slug>/` page resolves for
        // any slug and behind any subpath mount — the hub stores the CSS
        // untouched. No rewriting, ever: a rewrite is a hidden coupling to
        // the client's output format.
        let css = "@font-face{src:url(fonts/0)}";
        register_session(&st, &id, "http://h", reg(css)).unwrap();
        assert_eq!(
            st.sessions.lock().unwrap().get(&id).unwrap().css,
            css,
            "pushed CSS must be stored byte-for-byte"
        );
    }

    #[test]
    fn view_base_honors_forwarded_headers() {
        let cfg = "http://127.0.0.1:8080";

        // No proxy headers → configured base verbatim.
        assert_eq!(view_base(&HeaderMap::new(), cfg), cfg);

        // Host header only (no proxy) → configured scheme + that host.
        let mut h = HeaderMap::new();
        h.insert("host", "example.com".parse().unwrap());
        assert_eq!(view_base(&h, cfg), "http://example.com");

        // Full XFF chain → first token of each, overriding scheme + host.
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-proto", "https, http".parse().unwrap());
        h.insert(
            "x-forwarded-host",
            "hub.example.com, internal".parse().unwrap(),
        );
        h.insert("host", "internal:8080".parse().unwrap());
        assert_eq!(view_base(&h, cfg), "https://hub.example.com");

        // The headers ride the /push WebSocket upgrade, where a proxy reports
        // ws/wss — the view link is an HTTP(S) page, so the scheme maps over.
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-proto", "wss".parse().unwrap());
        h.insert("x-forwarded-host", "hub.example.com".parse().unwrap());
        assert_eq!(view_base(&h, cfg), "https://hub.example.com");
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-proto", "ws".parse().unwrap());
        h.insert("host", "example.com".parse().unwrap());
        assert_eq!(view_base(&h, cfg), "http://example.com");

        // A subpath-mounting proxy announces its prefix: the logged link must
        // spell it out (the pages themselves are prefix-agnostic).
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-proto", "https".parse().unwrap());
        h.insert("x-forwarded-host", "example.com".parse().unwrap());
        h.insert("x-forwarded-prefix", "/glass/".parse().unwrap());
        assert_eq!(view_base(&h, cfg), "https://example.com/glass");
    }

    // ── management API (router-level) ─────────────────────────────────────────

    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt as _;

    // The 426 contract the push client parses on a wire-protocol mismatch: status
    // 426, the hub's EXACT version, the inclusive range it serves, and a body that
    // names which side is behind. This replaces the old id-rotation 403 — SALT is
    // frozen, so a legit id still authorizes and reaches this check.
    #[tokio::test]
    async fn protocol_mismatch_reports_version_and_range() {
        // A client protocol above our max: the HUB is too old to serve it.
        let res = protocol_mismatch(999);
        assert_eq!(res.status(), StatusCode::UPGRADE_REQUIRED);
        let hv = |res: &Response, name: &str| {
            res.headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        };
        assert_eq!(
            hv(&res, proto::HUB_VERSION_HEADER).as_deref(),
            Some(env!("CARGO_PKG_VERSION")),
            "reports the hub's exact version",
        );
        assert_eq!(
            hv(&res, proto::PROTOCOL_MIN_HEADER),
            Some(proto::HUB_PROTOCOL_MIN.to_string()),
        );
        assert_eq!(
            hv(&res, proto::PROTOCOL_MAX_HEADER),
            Some(proto::PROTOCOL_VERSION.to_string()),
        );
        let body = axum::body::to_bytes(res.into_body(), 1 << 16)
            .await
            .unwrap();
        let body = String::from_utf8_lossy(&body);
        assert!(
            body.contains("Update the hub"),
            "client too new → names the hub: {body}"
        );

        // A client protocol below our min: the PUSH CLIENT is too old.
        let res = protocol_mismatch(0);
        let body = axum::body::to_bytes(res.into_body(), 1 << 16)
            .await
            .unwrap();
        assert!(
            String::from_utf8_lossy(&body).contains("Update the push client"),
            "client too old → names the push client",
        );
    }

    fn api_req(method: &str, uri: &str, bearer: Option<&str>, body: Option<&str>) -> Request<Body> {
        let mut b = Request::builder().method(method).uri(uri);
        if let Some(k) = bearer {
            b = b.header("authorization", format!("Bearer {k}"));
        }
        if body.is_some() {
            b = b.header("content-type", "application/json");
        }
        let mut req = b
            .body(body.map_or_else(Body::empty, |s| Body::from(s.to_string())))
            .unwrap();
        // Handlers extract ConnectInfo; a real server injects it, oneshot must.
        req.extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 9))));
        req
    }

    #[tokio::test]
    async fn api_namespace_hidden_until_configured() {
        let router = app(HubState::new(AllowConfig::default(), String::new()));
        for req in [
            api_req("GET", "/api/sessions", Some("whatever"), None),
            api_req("POST", "/api/sessions", Some("whatever"), Some("{}")),
            api_req("DELETE", "/api/sessions/by-slug/x", Some("whatever"), None),
        ] {
            let res = router.clone().oneshot(req).await.unwrap();
            assert_eq!(
                res.status(),
                StatusCode::NOT_FOUND,
                "no --api-allow = the namespace is a plain 404"
            );
        }
    }

    #[tokio::test]
    async fn api_auth_and_session_lifecycle() {
        let key = "api-secret";
        let st = HubState::new(AllowConfig::default(), String::new())
            .with_api_allowed([proto::api_id(key)]);
        let router = app(st.clone());
        let sid = session_id("pusher");

        // Missing and wrong credentials.
        let res = router
            .clone()
            .oneshot(api_req("GET", "/api/sessions", None, None))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        let res = router
            .clone()
            .oneshot(api_req("GET", "/api/sessions", Some("wrong"), None))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);

        // Add, aliased.
        let body = format!(r#"{{"id":"{sid}","slug":"pretty"}}"#);
        let res = router
            .clone()
            .oneshot(api_req("POST", "/api/sessions", Some(key), Some(&body)))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CREATED);
        assert!(st.is_allowed(&sid), "added session may push");

        // Duplicate id 409; malformed id 400.
        let res = router
            .clone()
            .oneshot(api_req("POST", "/api/sessions", Some(key), Some(&body)))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CONFLICT);
        let res = router
            .clone()
            .oneshot(api_req(
                "POST",
                "/api/sessions",
                Some(key),
                Some(r#"{"id":"not-hex"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);

        // List for reconciliation.
        let res = router
            .clone()
            .oneshot(api_req("GET", "/api/sessions", Some(key), None))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        let list: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(list[0]["id"], sid.as_str());
        assert_eq!(list[0]["slug"], "pretty");
        assert_eq!(list[0]["live"], false);

        // EXPLICIT namespaces: deleting an aliased session by its id-shaped
        // SLUG must miss — by-slug never falls back to the id namespace.
        let res = router
            .clone()
            .oneshot(api_req(
                "DELETE",
                &format!("/api/sessions/by-slug/{sid}"),
                Some(key),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        assert!(st.is_allowed(&sid), "wrong-namespace delete is a no-op");

        // Delete by slug; the session is gone from both namespaces.
        let res = router
            .clone()
            .oneshot(api_req(
                "DELETE",
                "/api/sessions/by-slug/pretty",
                Some(key),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        assert!(!st.is_allowed(&sid), "removed session may not push");
        let res = router
            .clone()
            .oneshot(api_req(
                "DELETE",
                "/api/sessions/by-slug/pretty",
                Some(key),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND, "second delete: gone");

        // Re-add un-aliased; delete by id.
        let res = router
            .clone()
            .oneshot(api_req(
                "POST",
                "/api/sessions",
                Some(key),
                Some(&format!(r#"{{"id":"{sid}"}}"#)),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CREATED);
        let res = router
            .clone()
            .oneshot(api_req(
                "DELETE",
                &format!("/api/sessions/by-id/{sid}"),
                Some(key),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
    }

    // A registered-but-unseeded session serves the built-in placeholder
    // (operator-offline) and transitions to the pushed page on registration.
    #[tokio::test]
    async fn unseeded_session_serves_placeholder_until_registered() {
        let sid = session_id("pusher");
        let st = HubState::new(
            parse_allow(&[format!("{sid}:demo")]).unwrap(),
            "http://h".into(),
        );
        let router = app(st.clone());

        // The slash-less form redirects RELATIVELY to the canonical
        // directory-shaped page URL (prefix-proxy safe), query preserved.
        let res = router
            .clone()
            .oneshot(Request::get("/s/demo?embed").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::PERMANENT_REDIRECT);
        assert_eq!(
            res.headers().get("location").and_then(|v| v.to_str().ok()),
            Some("demo/?embed"),
            "relative Location, query riding along"
        );

        // The view URL works before any pusher: built-in template + the
        // reload-on-operator-online observer (the placeholder's marker).
        let res = router
            .clone()
            .oneshot(Request::get("/s/demo/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK, "unseeded slug serves a page");
        let body = axum::body::to_bytes(res.into_body(), 1 << 22)
            .await
            .unwrap();
        let html = String::from_utf8_lossy(&body);
        assert!(
            html.contains("attributeFilter"),
            "placeholder carries the reload-on-online observer"
        );

        // Its SSE stream exists (the stub's offline Live) — status only, the
        // body is an endless stream.
        let res = router
            .clone()
            .oneshot(Request::get("/s/demo/events").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK, "unseeded SSE connects");

        // Unknown slugs stay hard 404s — 'waiting for operator' ≠ 'not a session'.
        let res = router
            .clone()
            .oneshot(Request::get("/s/nope/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);

        // ?embed serves the chrome-less embed template (fit-to-frame page, no
        // nav) — for the placeholder too, keeping the reload-on-online observer.
        let res = router
            .clone()
            .oneshot(Request::get("/s/demo/?embed").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), 1 << 22)
            .await
            .unwrap();
        let html = String::from_utf8_lossy(&body);
        assert!(html.contains("sg-offline"), "embed template served");
        assert!(!html.contains("<nav"), "embed page has no chrome");
        assert!(
            html.contains("attributeFilter"),
            "embedded placeholder still reloads when the operator arrives"
        );
        // Pages must stay subpath-mountable: no root-absolute URLs anywhere.
        for frag in ["src=\"/", "href=\"/", "url(/"] {
            assert!(
                !html.contains(frag),
                "embed page leaked a root-absolute URL ({frag})"
            );
        }
        // Unknown slugs 404 with ?embed too.
        let res = router
            .clone()
            .oneshot(Request::get("/s/nope/?embed").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);

        // The per-slug asset routes exist (the page references them relatively).
        for path in ["/s/demo/viewer.js", "/s/demo/favicon.svg"] {
            let res = router
                .clone()
                .oneshot(Request::get(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK, "{path}");
        }

        // The embedding shim is served (the snippet host pages reference) —
        // with ACAO *, or a cross-origin `type="module"` load is CORS-blocked.
        let res = router
            .clone()
            .oneshot(Request::get("/embed.js").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            res.headers()
                .get("access-control-allow-origin")
                .and_then(|v| v.to_str().ok()),
            Some("*"),
            "embed.js must be loadable as a cross-origin module"
        );

        // The pusher registers: the view now serves the pushed page, and the
        // placeholder observer is gone.
        register_session(&st, &sid, "http://h", reg(".pushed{}")).unwrap();
        let res = router
            .clone()
            .oneshot(Request::get("/s/demo/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), 1 << 22)
            .await
            .unwrap();
        let html = String::from_utf8_lossy(&body);
        assert!(
            html.contains(".pushed{}"),
            "registered view serves pushed CSS"
        );
        assert!(
            !html.contains("attributeFilter"),
            "no placeholder machinery on the real page"
        );
    }

    // The iframe-less embed data routes: config (the boot object) + style.css
    // (the pushed @font-face ONLY, so a light-DOM embed can't leak page CSS onto
    // the host), and the configurable CORS on them.
    #[tokio::test]
    async fn embed_data_routes_and_cors() {
        let sid = session_id("pusher");
        let st = HubState::new(
            parse_allow(&[format!("{sid}:demo")]).unwrap(),
            "http://h".into(),
        );
        let reg = proto::RegisterBody {
            css: "@font-face{BASE}\n.baseonly{color:red}".into(),
            font_css: "@font-face{ONLYFONTS}".into(),
            template: String::new(),
            render_cfg: r##"{"defFg":"#111","defBg":"#222","fillFont":"Foo","fontPx":14,"lhPx":16.8,"sym":[]}"##.into(),
            fonts: vec![],
        };
        register_session(&st, &sid, "http://h", reg).unwrap();

        // Default (no --cors-origin): routes work, no cross-origin header.
        let router = app(st.clone());
        let res = router
            .clone()
            .oneshot(Request::get("/s/demo/config").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let json = String::from_utf8_lossy(
            &axum::body::to_bytes(res.into_body(), 1 << 20)
                .await
                .unwrap(),
        )
        .into_owned();
        assert!(json.contains(r#""events":"events""#), "{json}");
        assert!(
            json.contains(r#""fillFont":"Foo""#),
            "pushed cfg rides: {json}"
        );
        assert!(json.contains(r#""proto":"#));

        // style.css = the pushed @font-face only, never the base rules.
        let res = router
            .clone()
            .oneshot(
                Request::get("/s/demo/style.css")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let css = String::from_utf8_lossy(
            &axum::body::to_bytes(res.into_body(), 1 << 20)
                .await
                .unwrap(),
        )
        .into_owned();
        assert!(css.contains("ONLYFONTS"), "serves font_css: {css}");
        assert!(
            !css.contains("baseonly"),
            "must not leak the base rules: {css}"
        );

        // Unknown slug 404s on both.
        for p in ["/s/nope/config", "/s/nope/style.css"] {
            let res = router
                .clone()
                .oneshot(Request::get(p).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::NOT_FOUND, "{p}");
        }

        // With --cors-origin: an allowed Origin is echoed on a data route, a
        // disallowed one is not.
        let router = app_with_cors(st, &["https://boss.example".to_string()]);
        let acao = |origin: &str| {
            let router = router.clone();
            let origin = origin.to_string();
            async move {
                let res = router
                    .oneshot(
                        Request::get("/s/demo/config")
                            .header("origin", origin)
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                res.headers()
                    .get("access-control-allow-origin")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string)
            }
        };
        assert_eq!(
            acao("https://boss.example").await.as_deref(),
            Some("https://boss.example"),
            "allowed origin echoed"
        );
        assert_eq!(
            acao("https://evil.example").await,
            None,
            "other origin denied"
        );
    }

    // Domain separation end to end: a key whose SESSION id is a registered
    // session must still be rejected by the API (its API id differs).
    #[tokio::test]
    async fn api_rejects_session_domain_keys() {
        let key = "double-duty-secret";
        let st = HubState::new(AllowConfig::default(), String::new())
            .with_api_allowed([proto::api_id("someone-else")]);
        st.add_session(&session_id(key), None).unwrap();
        let router = app(st);
        let res = router
            .oneshot(api_req("GET", "/api/sessions", Some(key), None))
            .await
            .unwrap();
        assert_eq!(
            res.status(),
            StatusCode::FORBIDDEN,
            "a session key must never authorize the management API"
        );
    }

    // --id-salt: a salted hub authorizes keys against EXT-derived ids only.
    // The same key's un-extended id must not pass — the extension is part of
    // the id ecosystem, not a cosmetic.
    #[tokio::test]
    async fn id_salt_extension_gates_api_auth() {
        let key = "api-secret";
        let st = HubState::new(AllowConfig::default(), String::new())
            .with_api_allowed([proto::api_id_ext(key, "hub-a")])
            .with_id_salt("hub-a".into());
        let res = app(st)
            .oneshot(api_req("GET", "/api/sessions", Some(key), None))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK, "ext-derived id authorizes");

        // Same key, but the allow-list holds the UN-extended id: rejected.
        let st = HubState::new(AllowConfig::default(), String::new())
            .with_api_allowed([proto::api_id(key)])
            .with_id_salt("hub-a".into());
        let res = app(st)
            .oneshot(api_req("GET", "/api/sessions", Some(key), None))
            .await
            .unwrap();
        assert_eq!(
            res.status(),
            StatusCode::FORBIDDEN,
            "un-extended id must not pass on a salted hub"
        );
    }
}

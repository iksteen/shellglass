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
use crate::fonts::CACHE_CONTROL_FONT;
use crate::model::Frame;
use crate::proto::{self, KEY_HEADER, session_id};
use crate::render;
use anyhow::{Context, Result, bail};
use axum::Router;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade, close_code};
use axum::extract::{ConnectInfo, Path, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{HeaderMap, StatusCode};
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
    /// Viewer template the client pushed (empty → the hub's built-in default).
    template: String,
    /// Render config the client pushed (colors + symbol_map) for its `viewer.js`.
    render_cfg: String,
    /// Live publisher: decoded pushed frames in, per-viewer cell deltas out.
    live: Arc<diff::Live>,
    /// Fonts the client uploaded, keyed as the CSS references them (`key` → (mime,
    /// bytes)). Scoped to this session so different clients' fonts never clash.
    fonts: HashMap<String, (String, Vec<u8>)>,
    /// Per-session kick: the management API's DELETE fires this so the live
    /// `/push` WebSocket Closes immediately (the pusher's next reconnect then
    /// 403s — its id is gone from the registry).
    kick: broadcast::Sender<()>,
}

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
    /// The session registry: which ids (`session_id(secret)`) may push and the
    /// public view slug each maps to. Seeded from `--allow`, mutable at runtime
    /// through the management API — every lookup takes a short read lock; the
    /// only writers are the API's add/remove handlers. The operator/API adds
    /// ids, never secrets — the hub screens by hash.
    registry: Arc<std::sync::RwLock<AllowConfig>>,
    /// Public base URL (`scheme://host:port`, no trailing slash) for logging the
    /// view URL when a new session connects.
    base: Arc<str>,
    /// Permits gating concurrent argon2 hashes (see [`HASH_SLOTS`]).
    hash_slots: Arc<Semaphore>,
    /// Fires once on SIGTERM: each open `/push` WebSocket sends a Close and returns
    /// so pushers detect the shutdown immediately (see `main`'s graceful path).
    shutdown: broadcast::Sender<()>,
}

impl HubState {
    pub fn new(allow: AllowConfig, base: String) -> Self {
        let (shutdown, _) = broadcast::channel(1);
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            registry: Arc::new(std::sync::RwLock::new(allow)),
            base: base.into(),
            hash_slots: Arc::new(Semaphore::new(HASH_SLOTS)),
            shutdown,
        }
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

    /// Every registered session as `(id, slug, live)` — `live` meaning an
    /// operator is currently pushing. For the management API's reconciliation
    /// listing.
    pub fn list_sessions(&self) -> Vec<(String, String, bool)> {
        let reg = self.registry.read().unwrap();
        let map = self.sessions.lock().unwrap();
        reg.by_id
            .iter()
            .map(|(id, slug)| {
                let live = map.get(id).is_some_and(|s| s.live.is_online());
                (id.clone(), slug.clone(), live)
            })
            .collect()
    }

    /// Drop a removed session's stored state (CSS/fonts/render-config/matrix)
    /// and kick its live pusher, if any. Viewer SSE streams end when the
    /// `Live` drops with them.
    fn drop_session_state(&self, id: &str) {
        let session = self.sessions.lock().unwrap().remove(id);
        if let Some(s) = session {
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
        tokio::task::spawn_blocking(move || session_id(&key))
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
    eprintln!(
        "shellglass: push auth failure {} on {route} client={client} peer={}",
        code.as_u16(),
        peer.ip()
    );
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
    // Compress the page + fonts, but never the SSE stream (compression buffers and
    // would defeat the realtime push). So layer per-route, not globally.
    let compress = CompressionLayer::new();
    Router::new()
        .route("/", get(index))
        // The push client's single WebSocket: register-then-stream state machine,
        // authorized once at the upgrade.
        .route("/push", get(ws_push))
        .route("/viewer.js", get(viewer_js).layer(compress.clone()))
        .route("/favicon.svg", get(favicon).layer(compress.clone()))
        .route("/s/{slug}", get(view).layer(compress.clone()))
        .route("/s/{slug}/events", get(events))
        .route("/s/{slug}/fonts/{key}", get(font).layer(compress))
        .with_state(state)
}

fn key_of(headers: &HeaderMap) -> Option<String> {
    headers.get(KEY_HEADER)?.to_str().ok().map(str::to_string)
}

/// Public base URL for logging a view link, honoring reverse-proxy headers so the
/// URL matches the address a viewer actually reaches (e.g. behind Traefik). Takes
/// scheme from `X-Forwarded-Proto`, host from `X-Forwarded-Host` then `Host`;
/// falls back to the configured base for whichever part is absent. XFF headers are
/// comma-lists (proxy chain) — the first token is the original client-facing value.
fn view_base(headers: &HeaderMap, configured: &str) -> String {
    let fwd = |name| {
        header_str(headers, name)
            .and_then(|v| v.split(',').next())
            .map(str::trim)
    };
    let (def_scheme, def_host) = configured
        .split_once("://")
        .map_or(("http", configured), |(s, h)| (s, h));
    let scheme = fwd("x-forwarded-proto")
        .filter(|s| !s.is_empty())
        .unwrap_or(def_scheme);
    let host = fwd("x-forwarded-host")
        .or_else(|| header_str(headers, "host"))
        .filter(|s| !s.is_empty())
        .unwrap_or(def_host);
    format!("{scheme}://{host}")
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name)?.to_str().ok()
}

/// The push client's WebSocket. Authorize once at the upgrade (moving the argon2
/// semaphore + fail2ban guards here — one hash per connection, not one per
/// register *and* one per stream), then run the register-then-stream state machine.
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
                        // Streaming: apply + forward. publish_wire drops malformed or
                        // out-of-sync messages rather than the whole session.
                        Some(l) => l.publish_wire(t.as_str()),
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

/// Create or refresh the session for `id` from a register message; returns its
/// `Live` plus a receiver for the session's kick channel (fired when the
/// management API deletes the session). New sessions get a "waiting…" banner
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
    // Decode uploaded fonts; silently drop any with bad base64 (the family just
    // falls back in the browser rather than failing the whole registration).
    let fonts: HashMap<String, (String, Vec<u8>)> = reg
        .fonts
        .into_iter()
        .filter_map(|f| Some((f.key, (f.mime, B64.decode(f.b64).ok()?))))
        .collect();
    // The id's public slug. Views live under the slug only, but the client baked
    // its `@font-face` URLs as `/s/<id>/fonts/…` (it can't know the hub's slug),
    // so rewrite them to the slug — otherwise fonts would 404 on an aliased
    // session. A no-op when slug == id (no alias). ponytail: coupled to client.rs
    // building exactly that prefix; the font route matches it too.
    let slug = st.slug_of(id)?;
    let css = reg
        .css
        .replace(&format!("/s/{id}/fonts/"), &format!("/s/{slug}/fonts/"));
    let mut map = st.sessions.lock().unwrap();
    if let Some(s) = map.get_mut(id) {
        s.css = css;
        s.template = reg.template;
        s.render_cfg = reg.render_cfg;
        s.fonts = fonts;
        // A reconnect: the operator is back (push_session marked it offline on the
        // previous drop). New sessions start online, so only this branch needs it.
        s.live.set_online(true);
        Some((Arc::clone(&s.live), s.kick.subscribe()))
    } else {
        let live = diff::Live::new(Arc::new(Frame::Banner(render::banner(
            "waiting for client…",
        ))));
        let (kick, kick_rx) = broadcast::channel(1);
        map.insert(
            id.to_string(),
            Session {
                css,
                template: reg.template,
                render_cfg: reg.render_cfg,
                live: Arc::clone(&live),
                fonts,
                kick,
            },
        );
        println!("shellglass: session connected — view at {base}/s/{slug}");
        Some((live, kick_rx))
    }
}

/// Serve a session's uploaded font bytes (the page's `@font-face` points here).
/// Public like `view`/`events` — the slug in the path is the read capability.
async fn font(State(st): State<HubState>, Path((slug, key)): Path<(String, String)>) -> Response {
    let id = st.id_of_view(&slug);
    let map = st.sessions.lock().unwrap();
    let Some(s) = id.and_then(|id| map.get(&id)) else {
        return (StatusCode::NOT_FOUND, "unknown session").into_response();
    };
    match s.fonts.get(&key) {
        // ponytail: clone the bytes per request; browsers cache fonts (see the
        // Cache-Control), so this is a cache-miss cost only. Wrap in Arc<[u8]> if it
        // ever shows up in a profile.
        Some((mime, bytes)) => (
            [
                (CONTENT_TYPE, mime.clone()),
                (CACHE_CONTROL, CACHE_CONTROL_FONT.to_string()),
            ],
            bytes.clone(),
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "unknown font").into_response(),
    }
}

async fn view(State(st): State<HubState>, Path(slug): Path<String>) -> Response {
    let id = st.id_of_view(&slug);
    let map = st.sessions.lock().unwrap();
    let Some(s) = id.and_then(|id| map.get(&id)) else {
        return (StatusCode::NOT_FOUND, "unknown session").into_response();
    };
    let script = render::sse_script(&format!("/s/{slug}/events"), &s.render_cfg);
    // Empty template = an older client that didn't push one; use the built-in.
    let template = if s.template.is_empty() {
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

    #[test]
    fn list_sessions_reports_registry() {
        let a = session_id("a");
        let st = HubState::new(AllowConfig::default(), String::new());
        st.add_session(&a, Some("alpha")).unwrap();
        let list = st.list_sessions();
        assert_eq!(list.len(), 1);
        let (id, slug, live) = &list[0];
        assert_eq!(id, &a);
        assert_eq!(slug, "alpha");
        assert!(!live, "no pusher has registered");
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
            template: String::new(),
            render_cfg: String::new(),
            fonts: vec![],
        }
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
        assert!(
            st.live(&id).is_none(),
            "no session before the first register"
        );

        // First register (the WS's first message) creates the session + its Live.
        let (live1, _kick1) = register_session(&st, &id, "http://h", reg("a{}")).unwrap();
        assert!(st.live(&id).is_some(), "session exists after register");

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

    #[test]
    fn register_rewrites_font_urls_to_the_slug() {
        let id = session_id("secret");
        let st = HubState::new(
            parse_allow(&[format!("{id}:pretty")]).unwrap(),
            "http://h".into(),
        );
        // The client bakes `/s/<id>/fonts/…`; the hub must rewrite it to the slug so
        // the sub-resource stays reachable under the slug-only view namespace.
        let css = format!("@font-face{{src:url(/s/{id}/fonts/0)}}");
        register_session(&st, &id, "http://h", reg(&css)).unwrap();
        assert_eq!(
            st.sessions.lock().unwrap().get(&id).unwrap().css,
            "@font-face{src:url(/s/pretty/fonts/0)}",
            "font URLs rewritten from id to slug"
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
    }
}

//! Multi-session hub server: receives pushes from clients and serves viewers.
//!
//! It renders nothing and re-diffs nothing. A client opens one WebSocket to `/push`
//! (keyed by `session_id(secret)`, authorized once at the upgrade) and runs a tiny
//! state machine over it: the **first** message is a [`proto::RegisterBody`] (page
//! CSS + render config + fonts), every message **after** is a pre-encoded wire
//! message (a full picture, then deltas). The hub applies each wire message to the
//! session's full matrix ([`diff::Live::publish_wire`], so late-joining viewers get
//! a correct snapshot) and forwards the bytes to viewers verbatim. Viewers open
//! `/s/<id>` and stream from `/s/<id>/events`. The id is the read capability; the
//! secret (never sent to viewers) is the write capability.
//!
//! One WebSocket (vs the old `/register` + `/stream` POST pair) means one auth, no
//! `409` ordering race, no length-framing layer, and — with a client ping/pong
//! heartbeat and a SIGTERM Close — prompt detection of a dead or restarting hub.

use crate::diff;
use crate::fonts::CACHE_CONTROL_FONT;
use crate::model::Frame;
use crate::proto::{self, KEY_HEADER, session_id};
use crate::render;
use axum::Router;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade, close_code};
use axum::extract::{ConnectInfo, Path, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use std::collections::{HashMap, HashSet};
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
}

/// Cap on concurrent `session_id` (argon2id) hashes. The hash is memory-hard
/// (~19 MiB, deliberately expensive) — unbounded concurrent grinding on bad keys
/// would exhaust memory and pin CPU, so authorize takes a permit before hashing.
/// Legitimate operators are a handful of allowlisted pushers reconnecting rarely,
/// so a small cap never contends; a flood just waits (and gets fail2ban'd). ponytail:
/// flat cap — raise it if legit operators ever queue behind each other.
const HASH_SLOTS: usize = 4;

#[derive(Clone)]
pub struct HubState {
    sessions: Arc<Mutex<HashMap<String, Session>>>,
    /// Pre-registered session ids (`session_id(secret)`) permitted to push. The
    /// operator adds ids, never secrets — the hub screens by hash.
    allowed: Arc<HashSet<String>>,
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
    pub fn new(allowed: HashSet<String>, base: String) -> Self {
        let (shutdown, _) = broadcast::channel(1);
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            allowed: Arc::new(allowed),
            base: base.into(),
            hash_slots: Arc::new(Semaphore::new(HASH_SLOTS)),
            shutdown,
        }
    }

    /// The `Live` publisher for a session id, if one exists (a client has registered).
    /// Used by the SSH viewer to resolve `ssh <id>@hub` to the session's frames.
    pub(crate) fn live(&self, id: &str) -> Option<Arc<diff::Live>> {
        let map = self.sessions.lock().unwrap();
        map.get(id).map(|s| Arc::clone(&s.live))
    }

    /// Signal every open `/push` WebSocket to close (graceful shutdown). Called from
    /// `main`'s SIGTERM handler; a no-op if no pushers are connected.
    pub fn trigger_shutdown(&self) {
        let _ = self.shutdown.send(());
    }
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
    if st.allowed.contains(&id) {
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
        // authorized once at the upgrade. Replaces the old /register + /stream POSTs.
        .route("/push", get(ws_push))
        .route("/viewer.js", get(viewer_js).layer(compress.clone()))
        .route("/favicon.svg", get(favicon).layer(compress.clone()))
        .route("/s/{id}", get(view).layer(compress.clone()))
        .route("/s/{id}/events", get(events))
        .route("/s/{id}/fonts/{key}", get(font).layer(compress))
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
    loop {
        tokio::select! {
            _ = shutdown.recv() => {
                let _ = socket.send(Message::Close(None)).await;
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
                        // rejected it at the header, so the body was never buffered.
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
                            Ok(reg) => live = Some(register_session(&st, &id, &base, reg)),
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

/// Create or refresh the session for `id` from a register message and return its
/// `Live`. New sessions get a "waiting…" banner (replaced by the first pushed frame)
/// and announce their view URL once — reconnects hit the refresh branch, so no spam.
fn register_session(
    st: &HubState,
    id: &str,
    base: &str,
    reg: proto::RegisterBody,
) -> Arc<diff::Live> {
    // Decode uploaded fonts; silently drop any with bad base64 (the family just
    // falls back in the browser rather than failing the whole registration).
    let fonts: HashMap<String, (String, Vec<u8>)> = reg
        .fonts
        .into_iter()
        .filter_map(|f| Some((f.key, (f.mime, B64.decode(f.b64).ok()?))))
        .collect();
    let mut map = st.sessions.lock().unwrap();
    if let Some(s) = map.get_mut(id) {
        s.css = reg.css;
        s.template = reg.template;
        s.render_cfg = reg.render_cfg;
        s.fonts = fonts;
        // A reconnect: the operator is back (push_session marked it offline on the
        // previous drop). New sessions start online, so only this branch needs it.
        s.live.set_online(true);
        Arc::clone(&s.live)
    } else {
        let live = diff::Live::new(Arc::new(Frame::Banner(render::banner(
            "waiting for client…",
        ))));
        map.insert(
            id.to_string(),
            Session {
                css: reg.css,
                template: reg.template,
                render_cfg: reg.render_cfg,
                live: Arc::clone(&live),
                fonts,
            },
        );
        println!("shellglass: session connected — view at {base}/s/{id}");
        live
    }
}

/// Serve a session's uploaded font bytes (the page's `@font-face` points here).
/// Public like `view`/`events` — the id in the path is the read capability.
async fn font(State(st): State<HubState>, Path((id, key)): Path<(String, String)>) -> Response {
    let map = st.sessions.lock().unwrap();
    let Some(s) = map.get(&id) else {
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

async fn view(State(st): State<HubState>, Path(id): Path<String>) -> Response {
    let map = st.sessions.lock().unwrap();
    let Some(s) = map.get(&id) else {
        return (StatusCode::NOT_FOUND, "unknown session").into_response();
    };
    let script = render::sse_script(&format!("/s/{id}/events"), &s.render_cfg);
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

async fn events(State(st): State<HubState>, Path(id): Path<String>) -> Response {
    let live = {
        let map = st.sessions.lock().unwrap();
        match map.get(&id) {
            Some(s) => Arc::clone(&s.live),
            None => return (StatusCode::NOT_FOUND, "unknown session").into_response(),
        }
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
    Html("<p style=\"font-family:monospace\">shellglass hub — open /s/&lt;session-id&gt;</p>")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::session_id;

    #[test]
    fn only_preregistered_keys_are_allowed() {
        let st = HubState::new(HashSet::from([session_id("good-secret")]), String::new());
        assert!(
            st.allowed.contains(&session_id("good-secret")),
            "registered key allowed"
        );
        assert!(
            !st.allowed.contains(&session_id("other-secret")),
            "unregistered key rejected"
        );
        // An empty allowlist rejects everything (no implicit open hub).
        let empty = HubState::new(HashSet::new(), String::new());
        assert!(!empty.allowed.contains(&session_id("good-secret")));
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
        let st = HubState::new(HashSet::from([id.clone()]), "http://h".into());
        assert!(
            st.live(&id).is_none(),
            "no session before the first register"
        );

        // First register (the WS's first message) creates the session + its Live.
        let live1 = register_session(&st, &id, "http://h", reg("a{}"));
        assert!(st.live(&id).is_some(), "session exists after register");

        // A reconnect re-registers: the CSS refreshes but the same Live is reused, so
        // viewers already subscribed don't get orphaned.
        let live2 = register_session(&st, &id, "http://h", reg("b{}"));
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

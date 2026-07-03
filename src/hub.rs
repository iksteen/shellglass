//! Multi-session hub server: receives pushes from clients and serves viewers.
//!
//! It renders nothing — a client pushes its already-rendered CSS (`/register`) then
//! streams fragments over a single persistent `/stream` POST, keyed by
//! `session_id(secret)`. Viewers open `/s/<id>` and stream updates from
//! `/s/<id>/events`. The id is the read capability; the secret (never sent to
//! viewers) is the write capability.

use crate::fonts::CACHE_CONTROL_FONT;
use crate::proto::{self, session_id, KEY_HEADER};
use crate::render;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;
use tokio_stream::wrappers::WatchStream;
use tokio_stream::StreamExt;
use tower_http::compression::CompressionLayer;

struct Session {
    css: String,
    /// Latest fragment; `watch` hands the current value to each new viewer.
    frame: watch::Sender<String>,
    /// Fonts the client uploaded, keyed as the CSS references them (`key` → (mime,
    /// bytes)). Scoped to this session so different clients' fonts never clash.
    fonts: HashMap<String, (String, Vec<u8>)>,
}

#[derive(Clone)]
pub struct HubState {
    sessions: Arc<Mutex<HashMap<String, Session>>>,
    /// Pre-registered session ids (`session_id(secret)`) permitted to push. The
    /// operator adds ids, never secrets — the hub screens by hash.
    allowed: Arc<HashSet<String>>,
}

impl HubState {
    pub fn new(allowed: HashSet<String>) -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            allowed: Arc::new(allowed),
        }
    }
}

/// Resolve a request's key to its (allowed) session id, or the status to reject
/// with: `401` if no key, `403` if the key isn't pre-registered on the hub.
/// Hashes the key once (Argon2 is deliberately expensive).
fn authorize(st: &HubState, headers: &HeaderMap) -> Result<String, StatusCode> {
    let key = key_of(headers).ok_or(StatusCode::UNAUTHORIZED)?;
    let id = session_id(&key);
    if st.allowed.contains(&id) {
        Ok(id)
    } else {
        Err(StatusCode::FORBIDDEN)
    }
}

pub fn app(state: HubState) -> Router {
    // Compress the page + fonts, but never the SSE stream (compression buffers and
    // would defeat the realtime push). So layer per-route, not globally.
    let compress = CompressionLayer::new();
    Router::new()
        .route("/", get(index))
        // /register carries the page CSS plus base64 fonts, which blows past axum's
        // 2 MB default. Allowed clients are trusted, so cap generously.
        .route(
            "/register",
            post(register).layer(DefaultBodyLimit::max(64 * 1024 * 1024)),
        )
        // /stream is an unbounded, never-ending push body — the default limit would
        // cut it off, so disable it here.
        .route("/stream", post(stream).layer(DefaultBodyLimit::disable()))
        .route("/s/{id}", get(view).layer(compress.clone()))
        .route("/s/{id}/events", get(events))
        .route("/s/{id}/fonts/{key}", get(font).layer(compress))
        .with_state(state)
}

fn key_of(headers: &HeaderMap) -> Option<String> {
    headers.get(KEY_HEADER)?.to_str().ok().map(str::to_string)
}

/// Store (or refresh) a session's CSS and served fonts. Creates the session if new.
async fn register(
    State(st): State<HubState>,
    headers: HeaderMap,
    Json(reg): Json<proto::RegisterBody>,
) -> Response {
    let id = match authorize(&st, &headers) {
        Ok(id) => id,
        Err(code) => return code.into_response(),
    };
    // Decode uploaded fonts; silently drop any with bad base64 (the family just
    // falls back in the browser rather than failing the whole registration).
    let fonts: HashMap<String, (String, Vec<u8>)> = reg
        .fonts
        .into_iter()
        .filter_map(|f| Some((f.key, (f.mime, B64.decode(f.b64).ok()?))))
        .collect();
    let mut map = st.sessions.lock().unwrap();
    match map.get_mut(&id) {
        Some(s) => {
            s.css = reg.css;
            s.fonts = fonts;
        }
        None => {
            let (frame, _) = watch::channel(render::banner("waiting for client…"));
            map.insert(id.clone(), Session { css: reg.css, frame, fonts });
        }
    }
    (StatusCode::OK, id).into_response()
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

/// Persistent push: the body is a stream of length-prefixed frames (see
/// [`proto::frame_encode`]). Each complete frame becomes the session's latest
/// screen. One connection carries the whole session, so pushes aren't gated by a
/// per-frame HTTP round-trip. `409` if the session hasn't been registered first.
async fn stream(State(st): State<HubState>, headers: HeaderMap, body: Body) -> Response {
    let id = match authorize(&st, &headers) {
        Ok(id) => id,
        Err(code) => return code.into_response(),
    };
    // Clone the watch sender out (don't hold the lock while streaming). send_replace
    // retains the latest frame even with no browser subscribed.
    let tx = match st.sessions.lock().unwrap().get(&id) {
        Some(s) => s.frame.clone(),
        None => return StatusCode::CONFLICT.into_response(),
    };

    let mut data = body.into_data_stream();
    let mut buf: Vec<u8> = Vec::new();
    while let Some(Ok(chunk)) = data.next().await {
        buf.extend_from_slice(&chunk);
        match proto::frame_drain(&mut buf) {
            Ok(frames) => {
                for f in frames {
                    tx.send_replace(f);
                }
            }
            Err(()) => break, // corrupt length prefix — drop the connection
        }
    }
    StatusCode::OK.into_response()
}

async fn view(State(st): State<HubState>, Path(id): Path<String>) -> Response {
    let map = st.sessions.lock().unwrap();
    let Some(s) = map.get(&id) else {
        return (StatusCode::NOT_FOUND, "unknown session").into_response();
    };
    let script = render::sse_script(&format!("/s/{id}/events"));
    Html(render::page(&s.css, &s.frame.borrow(), &script)).into_response()
}

async fn events(State(st): State<HubState>, Path(id): Path<String>) -> Response {
    let rx = {
        let map = st.sessions.lock().unwrap();
        match map.get(&id) {
            Some(s) => s.frame.subscribe(),
            None => return (StatusCode::NOT_FOUND, "unknown session").into_response(),
        }
    };
    let stream = WatchStream::new(rx).map(|html| Ok::<_, Infallible>(Event::default().data(html)));
    Sse::new(stream).keep_alive(KeepAlive::default()).into_response()
}

async fn index() -> Html<&'static str> {
    Html("<p style=\"font-family:monospace\">tmuxsnitch hub — open /s/&lt;session-id&gt;</p>")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::session_id;

    #[test]
    fn only_preregistered_keys_are_allowed() {
        let st = HubState::new(HashSet::from([session_id("good-secret")]));
        assert!(st.allowed.contains(&session_id("good-secret")), "registered key allowed");
        assert!(!st.allowed.contains(&session_id("other-secret")), "unregistered key rejected");
        // An empty allowlist rejects everything (no implicit open hub).
        let empty = HubState::new(HashSet::new());
        assert!(!empty.allowed.contains(&session_id("good-secret")));
    }
}

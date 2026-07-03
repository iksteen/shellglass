//! Multi-session hub server: receives pushes from clients and serves viewers.
//!
//! It renders nothing — a client pushes its already-rendered CSS (`/register`) then
//! streams fragments over a single persistent `/stream` POST, keyed by
//! `session_id(secret)`. Viewers open `/s/<id>` and stream updates from
//! `/s/<id>/events`. The id is the read capability; the secret (never sent to
//! viewers) is the write capability.

use crate::proto::{self, session_id, KEY_HEADER};
use crate::render;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;
use tokio_stream::wrappers::WatchStream;
use tokio_stream::StreamExt;

struct Session {
    css: String,
    /// Latest fragment; `watch` hands the current value to each new viewer.
    frame: watch::Sender<String>,
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

    fn allows(&self, key: &str) -> bool {
        self.allowed.contains(&session_id(key))
    }
}

/// Resolve a request's key to its (allowed) session id, or the status to reject
/// with: `401` if no key, `403` if the key isn't pre-registered on the hub.
fn authorize(st: &HubState, headers: &HeaderMap) -> Result<String, StatusCode> {
    let key = key_of(headers).ok_or(StatusCode::UNAUTHORIZED)?;
    if st.allows(&key) {
        Ok(session_id(&key))
    } else {
        Err(StatusCode::FORBIDDEN)
    }
}

pub fn app(state: HubState) -> Router {
    Router::new()
        .route("/", get(index))
        // /register carries the page CSS with base64-embedded fonts, which blows
        // past axum's 2 MB default. Allowed clients are trusted, so cap generously.
        .route(
            "/register",
            post(register).layer(DefaultBodyLimit::max(64 * 1024 * 1024)),
        )
        // /stream is an unbounded, never-ending push body — the default limit would
        // cut it off, so disable it here.
        .route("/stream", post(stream).layer(DefaultBodyLimit::disable()))
        .route("/s/{id}", get(view))
        .route("/s/{id}/events", get(events))
        .with_state(state)
}

fn key_of(headers: &HeaderMap) -> Option<String> {
    headers.get(KEY_HEADER)?.to_str().ok().map(str::to_string)
}

/// Store (or refresh) a session's CSS. Creates the session if new.
async fn register(State(st): State<HubState>, headers: HeaderMap, body: String) -> Response {
    let id = match authorize(&st, &headers) {
        Ok(id) => id,
        Err(code) => return code.into_response(),
    };
    let mut map = st.sessions.lock().unwrap();
    match map.get_mut(&id) {
        Some(s) => s.css = body,
        None => {
            let (frame, _) = watch::channel(render::banner("waiting for client…"));
            map.insert(id.clone(), Session { css: body, frame });
        }
    }
    (StatusCode::OK, id).into_response()
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
        let allowed = HashSet::from([session_id("good-secret")]);
        let st = HubState::new(allowed);
        assert!(st.allows("good-secret"), "registered key must be allowed");
        assert!(!st.allows("other-secret"), "unregistered key must be rejected");
        // An empty allowlist rejects everything (no implicit open hub).
        assert!(!HubState::new(HashSet::new()).allows("good-secret"));
    }
}

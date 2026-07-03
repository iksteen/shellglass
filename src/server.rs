//! Standalone live viewer: mirror local tmux to a local browser. A background
//! control-mode task (see [`crate::live`]) publishes fragments on a `watch`
//! channel; `GET /` serves the page and `GET /events` streams updates over SSE.

use crate::config::Config;
use crate::fonts::{FontFile, CACHE_CONTROL_FONT};
use crate::render;
use axum::extract::{Path, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use std::convert::Infallible;
use std::sync::Arc;
use tokio::sync::watch;
use tokio_stream::wrappers::WatchStream;
use tokio_stream::StreamExt;
use tower_http::compression::CompressionLayer;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub font_css: Arc<String>,
    /// Fonts served at `/fonts/<index>` so a remote browser renders them without a
    /// local install; the page's `@font-face` (in `font_css`) references these.
    pub fonts: Arc<Vec<FontFile>>,
    /// Latest rendered fragment, pushed by the live control task.
    pub live_rx: watch::Receiver<String>,
}

pub fn app(state: AppState) -> Router {
    // Compress the page + fonts, but never the SSE stream (compression buffers and
    // would defeat the realtime push). So layer per-route, not globally.
    let compress = CompressionLayer::new();
    Router::new()
        .route("/", get(index).layer(compress.clone()))
        .route("/events", get(events))
        .route("/fonts/{key}", get(font).layer(compress))
        .with_state(state)
}

async fn font(State(state): State<AppState>, Path(key): Path<String>) -> Response {
    match key.parse::<usize>().ok().and_then(|i| state.fonts.get(i)) {
        Some(f) => (
            [(CONTENT_TYPE, f.mime), (CACHE_CONTROL, CACHE_CONTROL_FONT)],
            f.bytes.clone(),
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn index(State(state): State<AppState>) -> Html<String> {
    Html(render::render_page(
        &state.live_rx.borrow(),
        &state.font_css,
        &state.config,
    ))
}

async fn events(State(state): State<AppState>) -> Response {
    let stream = WatchStream::new(state.live_rx.clone())
        .map(|html| Ok::<_, Infallible>(Event::default().data(html)));
    Sse::new(stream).keep_alive(KeepAlive::default()).into_response()
}

//! Standalone live viewer: mirror a local PTY command to a local browser. The PTY
//! backend (see [`crate::pty`]) publishes frames into a [`diff::Live`]; `GET /`
//! serves the page, `GET /events` streams cell deltas over SSE, and `GET /viewer.js`
//! serves the baked browser renderer.

use crate::config::Config;
use crate::fonts::{CACHE_CONTROL_FONT, FontFile};
use crate::model::Frame;
use crate::render;
use crate::{diff, fonts};
use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use std::sync::Arc;
use tower_http::compression::CompressionLayer;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    /// Symbol/font resolver, for the render config and the initial server-side paint.
    pub resolver: Arc<fonts::Resolver>,
    pub font_css: Arc<String>,
    /// Fonts served at `/fonts/<index>` so a remote browser renders them without a
    /// local install; the page's `@font-face` (in `font_css`) references these.
    pub fonts: Arc<Vec<FontFile>>,
    /// Viewer HTML template (`{{style}}`/`{{screen}}`/`{{script}}` tokens).
    pub template: Arc<String>,
    /// Live publisher: per-viewer SSE streams of cell deltas.
    pub live: Arc<diff::Live>,
}

pub fn app(state: AppState) -> Router {
    // Compress the page + fonts + renderer, but never the SSE stream (compression
    // buffers and would defeat the realtime push). So layer per-route, not globally.
    let compress = CompressionLayer::new();
    Router::new()
        .route("/", get(index).layer(compress.clone()))
        .route("/events", get(events))
        .route("/viewer.js", get(viewer_js).layer(compress.clone()))
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

/// Serve the baked renderer. `no-cache` (revalidate every load) since its bytes move
/// with the wire protocol on each upgrade — a stale copy would misparse new frames.
async fn viewer_js() -> Response {
    (
        [
            (CONTENT_TYPE, "application/javascript"),
            (CACHE_CONTROL, "no-cache"),
        ],
        render::VIEWER_JS,
    )
        .into_response()
}

async fn index(State(state): State<AppState>) -> Html<String> {
    // Initial paint from the current frame, so the page isn't blank before the first
    // SSE message; the renderer takes over (and rebuilds) as soon as it connects.
    let fragment = match &*state.live.current() {
        Frame::Screen(g) => render::render_fragment(g, &state.config, &state.resolver),
        Frame::Banner(html) => html.clone(),
    };
    let cfg = render::render_config_json(&state.config, &state.resolver);
    Html(render::render_page(
        &state.template,
        &fragment,
        &state.font_css,
        &state.config,
        &cfg,
    ))
}

async fn events(State(state): State<AppState>) -> Response {
    state.live.connect()
}

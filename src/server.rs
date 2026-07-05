//! Standalone live viewer: mirror a local PTY command to a local browser. The PTY
//! backend (see [`crate::pty`]) publishes frames into a [`diff::Live`]; `GET /`
//! serves the page, `GET /events` streams cell deltas over SSE, and `GET /viewer.js`
//! serves the baked browser renderer.

use crate::config::Config;
use crate::fonts::{CACHE_CONTROL_FONT, FontFile};
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
        .route("/favicon.svg", get(favicon).layer(compress.clone()))
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

/// Serve the baked renderer. The page references it by its content tag
/// (`/viewer.js?v=<tag>`), so the URL changes whenever the bytes do — safe to
/// cache forever, and a page reload after a server upgrade is guaranteed a
/// cache miss even behind a caching proxy.
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

async fn index(State(state): State<AppState>) -> Response {
    // The screen starts empty; the renderer paints it from the full frame that
    // heads the SSE stream, one round-trip after load (same as hub-served pages).
    let cfg = render::render_config_json(&state.config, &state.resolver);
    // no-cache: the auto-reload path depends on a reload fetching fresh HTML
    // (it carries the fingerprinted /viewer.js?v=… URL and the version pair).
    (
        [(CACHE_CONTROL, "no-cache")],
        Html(render::render_page(
            &state.template,
            &state.font_css,
            &state.config,
            &cfg,
        )),
    )
        .into_response()
}

async fn events(State(state): State<AppState>) -> Response {
    state.live.connect()
}

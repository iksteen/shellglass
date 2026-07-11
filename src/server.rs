//! Standalone live viewer: mirror a local PTY command to a local browser. The PTY
//! backend (see [`crate::pty`]) publishes frames into a [`diff::Live`]; `GET /`
//! serves the page, `GET /events` streams cell deltas over SSE, and `GET /viewer.js`
//! serves the baked browser renderer.

use crate::config::Config;
use crate::fonts::{CACHE_CONTROL_IMMUTABLE, FontFile};
use crate::render;
use crate::{diff, fonts};
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::http::header::{ACCESS_CONTROL_ALLOW_ORIGIN, CACHE_CONTROL, CONTENT_TYPE};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use std::collections::HashMap;
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
    /// Content-addressed inline-image payloads, serving `images/<key>` (fed
    /// from the frame stream in `run_serve`; on-screen keys are protected
    /// from eviction).
    pub images: Arc<std::sync::Mutex<diff::ImageStore>>,
}

pub fn app(state: AppState) -> Router {
    // Compress the page + fonts + renderer, but never the SSE stream (compression
    // buffers and would defeat the realtime push). So layer per-route, not globally.
    let compress = CompressionLayer::new();
    Router::new()
        .route("/", get(index).layer(compress.clone()))
        .route("/events", get(events))
        .route("/viewer.js", get(viewer_js).layer(compress.clone()))
        .route("/embed.js", get(embed_js).layer(compress.clone()))
        .route("/favicon.svg", get(favicon).layer(compress.clone()))
        .route("/fonts/{key}", get(font).layer(compress))
        // No compression: image formats are already compressed.
        .route("/images/{key}", get(image))
        .with_state(state)
}

/// Serve an inline-image payload (frame placements reference `images/<key>`).
/// The store is authoritative; the current frame's own `image_data` is the
/// fallback for the sliver between a frame publishing and the feed task
/// catching up. Content-addressed ⇒ immutable, cache forever.
async fn image(State(state): State<AppState>, Path(key): Path<String>) -> Response {
    let entry = state
        .images
        .lock()
        .unwrap()
        .get(&key)
        .or_else(|| match &*state.live.frame() {
            crate::model::Frame::Screen(g) => g
                .image_data
                .get(&key)
                .map(|b| (b.mime.clone(), b.bytes.clone())),
            crate::model::Frame::Banner(_) => None,
        });
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

async fn font(State(state): State<AppState>, Path(key): Path<String>) -> Response {
    // Fonts are addressed by content hash (FontFile::key), same as the CSS
    // references them; a handful of fonts makes the scan-with-rehash trivial.
    match state.fonts.iter().find(|f| f.key() == key) {
        Some(f) => (
            [
                (CONTENT_TYPE, f.mime),
                (CACHE_CONTROL, CACHE_CONTROL_IMMUTABLE),
            ],
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

async fn index(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    // The screen starts empty; the renderer paints it from the full frame that
    // heads the SSE stream, one round-trip after load (same as hub-served pages).
    let cfg = render::render_config_json(&state.config, &state.resolver);
    // `?embed`: the chrome-less fit-to-frame page (what an <iframe> shows) —
    // the built-in embed template instead of the configured one.
    let template = if params.contains_key("embed") {
        render::EMBED_TEMPLATE
    } else {
        &state.template
    };
    // no-cache: the auto-reload path depends on a reload fetching fresh HTML
    // (it carries the fingerprinted /viewer.js?v=… URL and the version pair).
    (
        [(CACHE_CONTROL, "no-cache")],
        Html(render::render_page(
            template,
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

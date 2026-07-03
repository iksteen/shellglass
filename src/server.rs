//! Standalone live viewer: mirror local tmux to a local browser. A background
//! control-mode task (see [`crate::live`]) publishes fragments on a `watch`
//! channel; `GET /` serves the page and `GET /events` streams updates over SSE.

use crate::config::Config;
use crate::render;
use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use std::convert::Infallible;
use std::sync::Arc;
use tokio::sync::watch;
use tokio_stream::wrappers::WatchStream;
use tokio_stream::StreamExt;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub font_css: Arc<String>,
    /// Latest rendered fragment, pushed by the live control task.
    pub live_rx: watch::Receiver<String>,
}

pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/events", get(events))
        .with_state(state)
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

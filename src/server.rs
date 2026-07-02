//! axum server: `GET /` serves the shell page + poller, `GET /snapshot` returns a
//! freshly captured fragment. tmux capture is blocking, so it runs on a blocking
//! thread.

use crate::config::Config;
use crate::fonts::Resolver;
use crate::{parse, render, tmux};
use axum::extract::State;
use axum::response::Html;
use axum::routing::get;
use axum::Router;
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub resolver: Arc<Resolver>,
    pub font_css: Arc<String>,
    pub target: Option<String>,
    pub interval_ms: u64,
}

pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/snapshot", get(snapshot))
        .with_state(state)
}

/// Capture + parse + render the pane fragment. Blocking; call via spawn_blocking.
fn capture_fragment(state: &AppState) -> anyhow::Result<String> {
    let raw = tmux::capture(state.target.as_deref())?;
    let window = parse::parse_window(raw);
    Ok(render::render_fragment(&window, &state.config, &state.resolver))
}

async fn fragment_or_banner(state: &AppState) -> String {
    let st = state.clone();
    let result = tokio::task::spawn_blocking(move || capture_fragment(&st)).await;
    match result {
        Ok(Ok(html)) => html,
        Ok(Err(e)) => banner(&e.to_string()),
        Err(e) => banner(&format!("capture task failed: {e}")),
    }
}

fn banner(msg: &str) -> String {
    let esc = msg
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    format!(
        "<div style=\"color:#ff6b6b;font-family:monospace;padding:8px;\">\
         tmuxsnitch: {esc}</div>"
    )
}

async fn index(State(state): State<AppState>) -> Html<String> {
    let fragment = fragment_or_banner(&state).await;
    Html(render::render_page(
        &fragment,
        &state.font_css,
        &state.config,
        state.interval_ms,
    ))
}

async fn snapshot(State(state): State<AppState>) -> Html<String> {
    Html(fragment_or_banner(&state).await)
}

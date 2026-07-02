//! tmuxsnitch — mirror a tmux window's full pane layout as live HTML.

mod config;
mod fonts;
mod model;
mod parse;
mod render;
mod server;
mod tmux;

use anyhow::{Context, Result};
use clap::Parser;
use config::Config;
use fonts::Resolver;
use server::AppState;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Parser, Debug)]
#[command(name = "tmuxsnitch", about = "Mirror a tmux window as live HTML")]
struct Args {
    /// tmux target (e.g. `session` or `session:window`); default = current window.
    #[arg(short, long)]
    target: Option<String>,

    /// Path to a TOML config (fonts + symbol_map). Optional.
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Address to bind the HTTP server.
    #[arg(short, long, default_value = "127.0.0.1:8080")]
    bind: String,

    /// Browser re-capture cadence, in milliseconds.
    #[arg(short, long, default_value_t = 500)]
    interval: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let config = match &args.config {
        Some(path) => Config::load(path)?,
        None => Config::default(),
    };
    let resolver = Resolver::build(&config).context("building font resolver")?;
    let font_css = fonts::font_face_css(&config).context("embedding fonts")?;

    let state = AppState {
        config: Arc::new(config),
        resolver: Arc::new(resolver),
        font_css: Arc::new(font_css),
        target: args.target.clone(),
        interval_ms: args.interval,
    };

    let listener = tokio::net::TcpListener::bind(&args.bind)
        .await
        .with_context(|| format!("binding {}", args.bind))?;
    let addr = listener.local_addr()?;
    println!(
        "tmuxsnitch: mirroring tmux target {:?} at http://{}/",
        args.target.as_deref().unwrap_or("<current>"),
        addr
    );

    axum::serve(listener, server::app(state)).await?;
    Ok(())
}

//! tmuxsnitch — mirror a tmux window's full pane layout as live HTML.

mod config;
mod fonts;
mod live;
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
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let config = match &args.config {
        Some(path) => Config::load(path)?,
        None => Config::default(),
    };
    let resolver = Arc::new(Resolver::build(&config).context("building font resolver")?);
    let font_css = fonts::font_face_css(&config).context("embedding fonts")?;
    let config = Arc::new(config);

    // Standalone live viewer: mirror local tmux to a local browser.
    let live_rx = live::start(args.target.clone(), config.clone(), resolver);
    let state = AppState {
        config,
        font_css: Arc::new(font_css),
        live_rx,
    };
    let listener = bind(&args.bind).await?;
    println!(
        "tmuxsnitch: mirroring tmux target {:?} (live) at http://{}/",
        args.target.as_deref().unwrap_or("<current>"),
        listener.local_addr()?
    );
    axum::serve(listener, server::app(state)).await?;
    Ok(())
}

/// Bind with `SO_REUSEADDR` so a restart can rebind immediately — otherwise the
/// previous run's connections linger in `TIME_WAIT` and the fresh bind fails with
/// "address in use" for up to a minute.
async fn bind(addr: &str) -> Result<tokio::net::TcpListener> {
    use tokio::net::TcpSocket;
    let sockaddr: std::net::SocketAddr = addr
        .parse()
        .with_context(|| format!("bind address must be IP:port, got {addr:?}"))?;
    let socket = if sockaddr.is_ipv6() {
        TcpSocket::new_v6()
    } else {
        TcpSocket::new_v4()
    }
    .context("creating socket")?;
    socket.set_reuseaddr(true)?;
    socket.bind(sockaddr).with_context(|| format!("binding {addr}"))?;
    socket.listen(1024).with_context(|| format!("listening on {addr}"))
}

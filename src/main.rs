//! tmuxsnitch — mirror a tmux window's full pane layout as live HTML.

mod client;
mod config;
mod fonts;
mod hub;
mod live;
mod model;
mod parse;
mod proto;
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

    /// Address to bind the HTTP server (standalone viewer or `--serve` hub).
    #[arg(short, long, default_value = "127.0.0.1:8080")]
    bind: String,

    /// Run as a hub: receive pushes from clients and serve their sessions.
    #[arg(long)]
    serve: bool,

    /// (Hub) A session id permitted to push; repeat for several. Pushes whose key
    /// doesn't hash to a listed id get 403. Compute an id with `--key K --print-id`.
    #[arg(long = "allow", value_name = "SESSION_ID")]
    allow: Vec<String>,

    /// Push to a hub at this base URL instead of serving locally (client mode).
    #[arg(long)]
    push: Option<String>,

    /// Secret key for `--push`. Its `sha256` is the shareable session id.
    /// (allow_hyphen_values: a secret may legitimately start with `-`.)
    #[arg(long, env = "TMUXSNITCH_KEY", allow_hyphen_values = true)]
    key: Option<String>,

    /// Print the session id for `--key` and exit (to add to a hub's `--allow`).
    #[arg(long)]
    print_id: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Helper: print a key's session id (for a hub operator's --allow list) and exit.
    if args.print_id {
        let key = args.key.context("--print-id requires --key (or TMUXSNITCH_KEY)")?;
        println!("{}", proto::session_id(&key));
        return Ok(());
    }

    // Hub needs no tmux/config: it only stores and re-serves what clients push.
    if args.serve {
        let allowed: std::collections::HashSet<String> = args.allow.into_iter().collect();
        if allowed.is_empty() {
            eprintln!(
                "tmuxsnitch: warning — no --allow session ids; the hub will reject all pushes (403)"
            );
        }
        let listener = bind(&args.bind).await?;
        println!("tmuxsnitch hub at http://{}/", listener.local_addr()?);
        axum::serve(listener, hub::app(hub::HubState::new(allowed))).await?;
        return Ok(());
    }

    // Standalone and client both render locally, so both load config + fonts.
    let config = match &args.config {
        Some(path) => Config::load(path)?,
        None => Config::default(),
    };
    let resolver = Arc::new(Resolver::build(&config).context("building font resolver")?);
    let font_css = fonts::font_face_css(&config).context("embedding fonts")?;
    let config = Arc::new(config);

    if let Some(url) = args.push {
        let key = args
            .key
            .context("--push requires --key (or the TMUXSNITCH_KEY env var)")?;
        return client::run(url, key, args.target, config, resolver, font_css).await;
    }

    // Standalone live viewer.
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

/// Bind with `SO_REUSEADDR` so a hub restart can rebind immediately — otherwise the
/// previous run's client/browser connections linger in `TIME_WAIT` and the fresh
/// bind fails with "address in use" for up to a minute.
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

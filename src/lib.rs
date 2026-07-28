//! shellglass — mirror an interactive terminal command as live HTML.
//!
//! One library, several binaries: the full multi-call `shellglass` CLI plus
//! slim per-mode executables (see the `[[bin]]` targets + `[features]` in
//! `Cargo.toml`). Every binary dispatches through [`cli`], so flags and
//! behavior can't drift between the full CLI and the per-mode ones.

// musl's default allocator serializes multithreaded allocation on one lock — a
// large regression for our threaded workload (tokio, the screen thread, the
// newest-wins image-encode worker, per-viewer broadcast buffers). Swap in
// mimalloc, but ONLY for the static musl release artifacts: glibc, macOS, and
// every dev/test build keep the system allocator, byte-for-byte unchanged.
// Defined here in the lib so all binaries (multi-call + per-mode) inherit it.
// https://nickb.dev/blog/default-musl-allocator-considered-harmful-to-performance/
#[cfg(target_env = "musl")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Build the CORS layer for the embed data routes from configured origins, or
/// `None` for the same-origin-only default. A single `*` allows any origin (no
/// credentials are ever used, so `*` is safe); otherwise only the exact listed
/// origins are echoed. Shared by the standalone server and the hub (both bring
/// in `tower-http`).
#[cfg(any(feature = "serve-api", feature = "hub"))]
pub(crate) fn server_cors(origins: &[String]) -> Option<tower_http::cors::CorsLayer> {
    use tower_http::cors::{AllowOrigin, CorsLayer};
    if origins.is_empty() {
        return None;
    }
    let allow = if origins.iter().any(|o| o == "*") {
        AllowOrigin::any()
    } else {
        AllowOrigin::list(origins.iter().filter_map(|o| o.parse().ok()))
    };
    Some(
        CorsLayer::new()
            .allow_methods([axum::http::Method::GET])
            .allow_origin(allow),
    )
}

/// Bind with `SO_REUSEADDR` so a restart can rebind immediately — otherwise the
/// previous run's client/browser connections linger in `TIME_WAIT` and the fresh
/// bind fails with "address in use" for up to a minute. Shared by every listener
/// the crate opens: the standalone/library server, the hub, and the SSH view.
#[cfg(any(feature = "serve-api", feature = "hub", feature = "ssh-view"))]
pub(crate) fn bind(addr: &str) -> anyhow::Result<tokio::net::TcpListener> {
    use anyhow::Context as _;
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
    socket
        .bind(sockaddr)
        .with_context(|| format!("binding {addr}"))?;
    socket
        .listen(1024)
        .with_context(|| format!("listening on {addr}"))
}

#[cfg(feature = "ssh-view")]
pub mod ansi;
#[cfg(feature = "presentation")]
pub mod api;
#[cfg(feature = "sessions")]
pub mod apictl;
pub mod cli;
#[cfg(feature = "push-api")]
pub mod client;
#[cfg(any(feature = "sessions", feature = "recordings"))]
pub(crate) mod cliutil;
#[cfg(feature = "presentation")]
pub mod config;
pub mod diff;
pub mod fonts;
#[cfg(feature = "hub")]
pub mod hub;
#[cfg(feature = "mirror")]
pub mod images;
pub mod model;
#[cfg(feature = "mirror")]
pub mod parse;
pub mod proto;
#[cfg(feature = "mirror")]
pub mod pty;
#[cfg(feature = "recordings")]
pub mod recctl;
#[cfg(any(feature = "serve-api", feature = "hub"))]
pub mod record;
pub mod render;
#[cfg(feature = "serve-api")]
pub mod server;
pub mod source;
#[cfg(feature = "ssh-view")]
pub mod ssh;

//! shellglass — mirror a tmux window's full pane layout as live HTML.

mod client;
mod config;
mod fonts;
mod hub;
mod live;
mod model;
mod parse;
mod proto;
mod pty;
mod render;
mod server;
mod tmux;

use anyhow::{Context, Result};
use clap::Parser;
use config::Config;
use fonts::{FontFile, Resolver};
use server::AppState;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Parser, Debug)]
#[command(name = "shellglass", about = "Mirror a tmux window as live HTML")]
struct Cli {
    #[command(subcommand)]
    action: Action,
}

/// Each variant is one self-contained mode; clap only accepts the flags that belong
/// to the chosen subcommand, so incompatible options can't be combined by construction.
#[derive(clap::Subcommand, Debug)]
enum Action {
    /// Generate a secure random secret key, print it with its session id, and exit.
    GenKey,

    /// Print the session id for a key (to add to a hub's `hub --allow`).
    PrintId {
        #[command(flatten)]
        key: KeyArg,
    },

    /// Mirror a terminal locally: serve the live HTML viewer over HTTP (self-contained).
    Serve {
        #[command(flatten)]
        source: SourceArgs,

        /// Address to bind the HTTP server.
        #[arg(short, long, default_value = "127.0.0.1:8080")]
        bind: String,
    },

    /// Mirror a terminal and push frames to a remote hub instead of serving locally.
    Push {
        /// Hub base URL to push to (e.g. `https://hub.example.com`).
        url: String,

        #[command(flatten)]
        key: KeyArg,

        #[command(flatten)]
        source: SourceArgs,
    },

    /// Run as a hub: receive pushes from clients and re-serve their sessions.
    Hub(HubArgs),
}

/// The terminal source, shared by `view` and `push` (both render locally).
#[derive(clap::Args, Debug)]
struct SourceArgs {
    /// tmux target (e.g. `session` or `session:window`); default = current window.
    #[arg(short, long)]
    target: Option<String>,

    /// Mirror an interactive command in a PTY instead of tmux (the `script(1)`
    /// model): the command runs in your terminal, the browser watches. Everything
    /// after it is the command + args, so put it last, e.g. `--exec bash -l`.
    #[arg(long, num_args = 1.., allow_hyphen_values = true, value_name = "CMD")]
    exec: Vec<String>,

    /// Path to a TOML config (fonts + `symbol_map`). Optional.
    #[arg(short, long)]
    config: Option<PathBuf>,
}

/// Secret key whose `argon2id` hash is the shareable session id, shared by
/// `print-id` and `push`. (`allow_hyphen_values`: a secret may start with `-`.)
#[derive(clap::Args, Debug)]
struct KeyArg {
    #[arg(long, env = "SHELLGLASS_KEY", allow_hyphen_values = true)]
    key: String,
}

#[derive(clap::Args, Debug)]
struct HubArgs {
    /// Address to bind the hub's HTTP(S) server.
    #[arg(short, long, default_value = "127.0.0.1:8080")]
    bind: String,

    /// A session id permitted to push; repeat for several. Pushes whose key doesn't
    /// hash to a listed id get 403. Compute an id with `print-id --key K`.
    #[arg(long = "allow", value_name = "SESSION_ID")]
    allow: Vec<String>,

    /// Serve HTTPS with this certificate chain (PEM). Requires --tls-key.
    #[arg(long, requires = "tls_key")]
    tls_cert: Option<PathBuf>,

    /// Private key (PEM) for --tls-cert.
    #[arg(long, requires = "tls_cert")]
    tls_key: Option<PathBuf>,

    /// Obtain/renew a certificate automatically via ACME for this domain;
    /// repeat for several. Mutually exclusive with --tls-cert.
    #[arg(long = "acme-domain", value_name = "DOMAIN")]
    acme_domain: Vec<String>,

    /// Contact email for the ACME account.
    #[arg(long)]
    acme_email: Option<String>,

    /// Directory to persist the ACME account + issued certificates across
    /// restarts. Strongly recommended — without it certs are re-issued every run.
    #[arg(long)]
    acme_cache: Option<PathBuf>,

    /// Use the Let's Encrypt production directory (default: staging).
    #[arg(long)]
    acme_production: bool,
}

/// How the hub should terminate TLS.
enum Tls {
    None,
    Static {
        cert: PathBuf,
        key: PathBuf,
    },
    Acme {
        domains: Vec<String>,
        email: Option<String>,
        cache: Option<PathBuf>,
        production: bool,
    },
}

impl Tls {
    fn from_args(a: &HubArgs) -> Result<Tls> {
        let static_tls = a.tls_cert.is_some(); // clap guarantees tls_key is paired
        let acme = !a.acme_domain.is_empty();
        if static_tls && acme {
            anyhow::bail!("use either --tls-cert/--tls-key or --acme-domain, not both");
        }
        if static_tls {
            return Ok(Tls::Static {
                cert: a.tls_cert.clone().unwrap(),
                key: a.tls_key.clone().unwrap(),
            });
        }
        if acme {
            return Ok(Tls::Acme {
                domains: a.acme_domain.clone(),
                email: a.acme_email.clone(),
                cache: a.acme_cache.clone(),
                production: a.acme_production,
            });
        }
        Ok(Tls::None)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().action {
        Action::GenKey => gen_key(),
        Action::PrintId { key } => {
            println!("{}", proto::session_id(&key.key));
            Ok(())
        }
        Action::Serve { source, bind } => run_serve(source, &bind).await,
        Action::Push { url, key, source } => run_push(url, key.key, source).await,
        Action::Hub(hub) => {
            let tls = Tls::from_args(&hub)?;
            let allowed: std::collections::HashSet<String> = hub.allow.into_iter().collect();
            if allowed.is_empty() {
                eprintln!(
                    "shellglass: warning — no --allow session ids; the hub will reject all pushes (403)"
                );
            }
            serve_hub(allowed, &hub.bind, tls).await
        }
    }
}

/// Mint a new secret key (32 bytes of OS randomness, URL-safe base64) and print it
/// with its session id. The key is the write capability (keep it secret); the id is
/// the read capability to put on the hub's `hub --allow`.
fn gen_key() -> Result<()> {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|e| anyhow::anyhow!("reading OS randomness for the new key: {e}"))?;
    let key = base64::Engine::encode(&URL_SAFE_NO_PAD, bytes);
    println!("key: {key}");
    println!("id:  {}", proto::session_id(&key));
    Ok(())
}

/// The shared render setup for `view` and `push`: load config, resolve fonts, and
/// build the input backend. Returns everything the two modes need to run.
struct Rendered {
    config: Arc<Config>,
    fonts: Arc<Vec<FontFile>>,
    template: Arc<String>,
    rx: tokio::sync::watch::Receiver<String>,
    notifier: Option<pty::Notifier>,
}

/// Load config + fonts and start the input backend (PTY if `--exec`, else tmux). The
/// caller prints its URL *before* this returns a PTY backend, because a PTY switches
/// the terminal to raw mode and anything printed after would corrupt the session.
fn render_setup(source: SourceArgs) -> Result<Rendered> {
    let mut config = match &source.config {
        Some(path) => Config::load(path)?,
        None => Config::default(),
    };
    let resolver = Arc::new(Resolver::build(&config).context("building font resolver")?);
    // Pin any generic (monospace/…) in default_font to the host's concrete font so
    // viewers see the same face, then locate + read every referenced font on this
    // host (which has them installed) so we can serve them to viewers that don't.
    fonts::resolve_generics(&mut config);
    let fonts = Arc::new(fonts::collect_fonts(&config));
    let template = Arc::new(config.template_html().context("loading viewer template")?);
    let config = Arc::new(config);

    let (rx, notifier) = if source.exec.is_empty() {
        (live::start(source.target, config.clone(), resolver), None)
    } else {
        let (rx, n) = pty::start(&source.exec, config.clone(), resolver)?;
        (rx, Some(n))
    };
    Ok(Rendered {
        config,
        fonts,
        template,
        rx,
        notifier,
    })
}

/// Standalone live viewer: render locally and serve the page + SSE over HTTP.
async fn run_serve(source: SourceArgs, bind_addr: &str) -> Result<()> {
    let listener = bind(bind_addr)?;
    let desc = describe_source(&source);
    let r = render_setup(source)?;
    // Print the URL before a PTY backend switches the terminal to raw mode.
    println!(
        "shellglass: mirroring {desc} at http://{}/",
        listener.local_addr()?
    );
    let state = AppState {
        font_css: Arc::new(render::font_face_css(&r.fonts, "/fonts/")),
        config: r.config,
        fonts: r.fonts,
        template: r.template,
        live_rx: r.rx,
    };
    axum::serve(listener, server::app(state)).await?;
    Ok(())
}

/// Client mode: render locally and push frames to a remote hub.
async fn run_push(url: String, key: String, source: SourceArgs) -> Result<()> {
    let id = proto::session_id(&key);
    let base = url.trim_end_matches('/');
    // Print the view URL before starting the backend (PTY raw mode) — see render_setup.
    println!("shellglass: pushing live to {base}; view at {base}/s/{id}");
    let r = render_setup(source)?;
    client::run(
        url, key, id, r.config, r.fonts, r.template, r.rx, r.notifier,
    )
    .await
}

/// One-line description of what a source mirrors, for the startup log.
fn describe_source(source: &SourceArgs) -> String {
    if source.exec.is_empty() {
        format!(
            "tmux target {:?} (live)",
            source.target.as_deref().unwrap_or("<current>")
        )
    } else {
        format!("`{}` (pty)", source.exec.join(" "))
    }
}

/// Serve the hub, terminating TLS per `tls`. Plain HTTP keeps the `SO_REUSEADDR`
/// listener via `axum::serve`; the TLS paths hand the same reuseaddr listener to
/// `axum-server`. ACME drives certificate issuance/renewal on a background task.
async fn serve_hub(allowed: std::collections::HashSet<String>, addr: &str, tls: Tls) -> Result<()> {
    let listener = bind(addr)?;
    let local = listener.local_addr()?;
    // Public base for the view URLs the hub logs. For ACME the cert is for the
    // domain, so use that; otherwise the bound address (as in the startup line).
    let base = match &tls {
        Tls::None => format!("http://{local}"),
        Tls::Static { .. } => format!("https://{local}"),
        Tls::Acme { domains, .. } => {
            format!(
                "https://{}",
                domains.first().map_or("localhost", String::as_str)
            )
        }
    };
    let app = hub::app(hub::HubState::new(allowed, base));
    match tls {
        Tls::None => {
            println!("shellglass hub at http://{local}/");
            axum::serve(listener, app).await?;
        }
        Tls::Static { cert, key } => {
            use axum_server::tls_rustls::RustlsConfig;
            let config = RustlsConfig::from_pem_file(&cert, &key)
                .await
                .with_context(|| format!("loading TLS cert {cert:?} + key {key:?}"))?;
            let std_listener = listener.into_std()?;
            println!("shellglass hub at https://{local}/");
            axum_server::from_tcp_rustls(std_listener, config)?
                .serve(app.into_make_service())
                .await?;
        }
        Tls::Acme {
            domains,
            email,
            cache,
            production,
        } => {
            use rustls_acme::{AcmeConfig, caches::DirCache};
            use tokio_stream::StreamExt;
            if cache.is_none() {
                eprintln!(
                    "shellglass: warning — no --acme-cache; certificate + account are re-issued \
                     every run (Let's Encrypt will rate-limit you). Set --acme-cache DIR."
                );
            }
            let mut acme = AcmeConfig::new(domains.clone())
                .directory_lets_encrypt(production)
                .cache_option(cache.map(DirCache::new));
            if let Some(e) = email {
                acme = acme.contact_push(format!("mailto:{e}"));
            }
            let mut state = acme.state();
            let acceptor = state.axum_acceptor(state.default_rustls_config());
            // ACME (challenge, issuance, renewal) only advances while this stream is
            // polled — drive it forever on its own task.
            tokio::spawn(async move {
                while let Some(ev) = state.next().await {
                    match ev {
                        Ok(ok) => eprintln!("shellglass acme: {ok:?}"),
                        Err(err) => eprintln!("shellglass acme error: {err}"),
                    }
                }
            });
            let std_listener = listener.into_std()?;
            let env = if production { "production" } else { "staging" };
            println!("shellglass hub at https://{local}/ (ACME {env}: {domains:?})");
            axum_server::from_tcp(std_listener)?
                .acceptor(acceptor)
                .serve(app.into_make_service())
                .await?;
        }
    }
    Ok(())
}

/// Bind with `SO_REUSEADDR` so a hub restart can rebind immediately — otherwise the
/// previous run's client/browser connections linger in `TIME_WAIT` and the fresh
/// bind fails with "address in use" for up to a minute.
fn bind(addr: &str) -> Result<tokio::net::TcpListener> {
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

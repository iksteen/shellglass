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
use fonts::Resolver;
use server::AppState;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Parser, Debug)]
#[command(name = "shellglass", about = "Mirror a tmux window as live HTML")]
struct Args {
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

    /// Secret key for `--push`. Its `argon2id` hash is the shareable session id.
    /// (`allow_hyphen_values`: a secret may legitimately start with `-`.)
    #[arg(long, env = "SHELLGLASS_KEY", allow_hyphen_values = true)]
    key: Option<String>,

    /// Print the session id for `--key` and exit (to add to a hub's `--allow`).
    #[arg(long)]
    print_id: bool,

    /// (Hub) Serve HTTPS with this certificate chain (PEM). Requires --tls-key.
    #[arg(long, requires = "tls_key")]
    tls_cert: Option<PathBuf>,

    /// (Hub) Private key (PEM) for --tls-cert.
    #[arg(long, requires = "tls_cert")]
    tls_key: Option<PathBuf>,

    /// (Hub) Obtain/renew a certificate automatically via ACME for this domain;
    /// repeat for several. Mutually exclusive with --tls-cert.
    #[arg(long = "acme-domain", value_name = "DOMAIN")]
    acme_domain: Vec<String>,

    /// (Hub) Contact email for the ACME account.
    #[arg(long)]
    acme_email: Option<String>,

    /// (Hub) Directory to persist the ACME account + issued certificates across
    /// restarts. Strongly recommended — without it certs are re-issued every run.
    #[arg(long)]
    acme_cache: Option<PathBuf>,

    /// (Hub) Use the Let's Encrypt production directory (default: staging).
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
    fn from_args(a: &Args) -> Result<Tls> {
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
    let args = Args::parse();

    // Helper: print a key's session id (for a hub operator's --allow list) and exit.
    if args.print_id {
        let key = args
            .key
            .context("--print-id requires --key (or SHELLGLASS_KEY)")?;
        println!("{}", proto::session_id(&key));
        return Ok(());
    }

    // Hub needs no tmux/config: it only stores and re-serves what clients push.
    if args.serve {
        let tls = Tls::from_args(&args)?;
        let allowed: std::collections::HashSet<String> = args.allow.into_iter().collect();
        if allowed.is_empty() {
            eprintln!(
                "shellglass: warning — no --allow session ids; the hub will reject all pushes (403)"
            );
        }
        serve_hub(allowed, &args.bind, tls).await?;
        return Ok(());
    }

    // Standalone and client both render locally, so both load config + fonts.
    let mut config = match &args.config {
        Some(path) => Config::load(path)?,
        None => Config::default(),
    };
    let resolver = Arc::new(Resolver::build(&config).context("building font resolver")?);
    // Pin any generic (monospace/…) in default_font to the host's concrete font so
    // viewers see the same face, then locate + read every referenced font on this
    // host (which has them installed) so we can serve them to viewers that don't.
    // Done once here for both modes.
    fonts::resolve_generics(&mut config);
    let fonts = Arc::new(fonts::collect_fonts(&config));
    let template = Arc::new(config.template_html().context("loading viewer template")?);
    let config = Arc::new(config);

    let interactive = !args.exec.is_empty();

    if let Some(url) = args.push {
        let key = args
            .key
            .context("--push requires --key (or the SHELLGLASS_KEY env var)")?;
        // Print the view URL *before* starting the backend: a PTY backend switches
        // the terminal to raw mode, so anything printed after would land in — and
        // corrupt — the mirrored session.
        let id = proto::session_id(&key);
        let base = url.trim_end_matches('/');
        println!("shellglass: pushing live to {base}; view at {base}/s/{id}");
        let (rx, notifier) = start_backend(
            interactive,
            &args.exec,
            args.target.clone(),
            config.clone(),
            resolver,
        )?;
        return client::run(url, key, id, config, fonts, template, rx, notifier).await;
    }

    // Standalone live viewer: serve fonts at /fonts/<index>.
    let font_css = render::font_face_css(&fonts, "/fonts/");
    let listener = bind(&args.bind)?;
    // Print the URL before a PTY backend switches the terminal to raw mode.
    if interactive {
        println!(
            "shellglass: mirroring `{}` (pty) at http://{}/",
            args.exec.join(" "),
            listener.local_addr()?
        );
    } else {
        println!(
            "shellglass: mirroring tmux target {:?} (live) at http://{}/",
            args.target.as_deref().unwrap_or("<current>"),
            listener.local_addr()?
        );
    }
    let (live_rx, _notifier) = start_backend(
        interactive,
        &args.exec,
        args.target.clone(),
        config.clone(),
        resolver,
    )?;
    let state = AppState {
        config,
        font_css: Arc::new(font_css),
        fonts,
        template,
        live_rx,
    };
    axum::serve(listener, server::app(state)).await?;
    Ok(())
}

/// Pick the input backend: an interactive PTY command (`--exec`) or tmux control
/// mode. Both yield a `watch::Receiver` of the latest rendered `#screen` fragment.
fn start_backend(
    interactive: bool,
    exec: &[String],
    target: Option<String>,
    config: Arc<Config>,
    resolver: Arc<Resolver>,
) -> Result<(tokio::sync::watch::Receiver<String>, Option<pty::Notifier>)> {
    if interactive {
        let (rx, notifier) = pty::start(exec, config, resolver)?;
        Ok((rx, Some(notifier)))
    } else {
        Ok((live::start(target, config, resolver), None))
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

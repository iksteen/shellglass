//! shellglass — mirror an interactive terminal command as live HTML.

mod ansi;
mod client;
mod config;
mod diff;
mod fonts;
mod hub;
mod model;
mod parse;
mod proto;
mod pty;
mod render;
mod server;
mod ssh;

use anyhow::{Context, Result};
use clap::Parser;
use config::Config;
use fonts::{FontFile, Resolver};
use server::AppState;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Parser, Debug)]
#[command(
    name = "shellglass",
    version,
    about = "Mirror an interactive terminal command as live HTML"
)]
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

        /// Also serve a read-only ANSI view over SSH on this address (e.g.
        /// `127.0.0.1:2222`). Any username connects — `ssh -p 2222 x@host`.
        #[arg(long)]
        ssh_bind: Option<String>,

        /// OpenSSH-format host key for the SSH view. Generated + persisted (0600) at
        /// this path on first run; without it, a key under `$XDG_STATE_HOME` is used.
        #[arg(long)]
        ssh_host_key: Option<PathBuf>,
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

/// The terminal source, shared by `serve` and `push` (both render locally).
#[derive(clap::Args, Debug)]
struct SourceArgs {
    /// Path to a TOML config (fonts + `symbol_map`). Optional.
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Interactive command to mirror in a PTY (the `script(1)` model): it runs in
    /// your terminal, the browser watches. Put it last, after any flags — e.g.
    /// `serve -- bash -l`. Defaults to your `$SHELL` when omitted.
    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        value_name = "CMD"
    )]
    command: Vec<String>,
}

impl SourceArgs {
    /// The command to run, defaulting to the user's `$SHELL` (then `/bin/sh`).
    fn command(&self) -> Vec<String> {
        if self.command.is_empty() {
            vec![std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())]
        } else {
            self.command.clone()
        }
    }
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

    /// Also serve a read-only ANSI view over SSH on this address (e.g.
    /// `0.0.0.0:2222`). Connect with the session id as the username:
    /// `ssh -p 2222 <session-id>@host`.
    #[arg(long)]
    ssh_bind: Option<String>,

    /// OpenSSH-format host key for the SSH view. Generated + persisted (0600) at
    /// this path on first run; without it, a key under `$XDG_STATE_HOME` is used.
    #[arg(long)]
    ssh_host_key: Option<PathBuf>,
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
        Action::Serve {
            source,
            bind,
            ssh_bind,
            ssh_host_key,
        } => run_serve(source, &bind, ssh_bind, ssh_host_key).await,
        Action::Push { url, key, source } => run_push(url, key.key, source).await,
        Action::Hub(hub) => {
            let tls = Tls::from_args(&hub)?;
            let allowed: std::collections::HashSet<String> = hub.allow.into_iter().collect();
            if allowed.is_empty() {
                eprintln!(
                    "shellglass: warning — no --allow session ids; the hub will reject all pushes (403)"
                );
            }
            serve_hub(allowed, &hub.bind, tls, hub.ssh_bind, hub.ssh_host_key).await
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

/// Config-derived state shared by `serve` and `push`: fonts resolved and read,
/// template loaded. Deliberately does NOT start the PTY — `push` must register
/// with the hub first, so a bad hub address or a down hub is reported (and
/// retried) before the terminal is switched to raw mode and the command runs.
struct Setup {
    config: Arc<Config>,
    resolver: Arc<Resolver>,
    fonts: Arc<Vec<FontFile>>,
    template: Arc<String>,
}

fn setup(source: &SourceArgs) -> Result<Setup> {
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
    Ok(Setup {
        config: Arc::new(config),
        resolver,
        fonts,
        template,
    })
}

/// Standalone live viewer: render locally and serve the page + SSE over HTTP (and,
/// with `--ssh-bind`, a read-only ANSI view over SSH). When the mirrored command
/// exits, `pty.rs` exits the whole process, so the SSH connection drops and the
/// client's own tty is restored by its ssh.
async fn run_serve(
    source: SourceArgs,
    bind_addr: &str,
    ssh_bind: Option<String>,
    ssh_host_key: Option<PathBuf>,
) -> Result<()> {
    let listener = bind(bind_addr)?;
    let s = setup(&source)?;
    // Bind + resolve the SSH host key before the PTY takes the terminal, so the hint
    // and fingerprint print cleanly (raw mode hasn't started yet). A failure here must
    // NOT abort the HTTP mirror — log and continue without the SSH view.
    let ssh_ready = match &ssh_bind {
        Some(addr) => match prepare_ssh(addr, ssh_host_key.as_deref(), "x") {
            Ok(ready) => Some(ready),
            Err(e) => {
                eprintln!("shellglass: SSH view disabled — {e:#}");
                None
            }
        },
        None => None,
    };
    // Print the URL before the PTY switches the terminal to raw mode.
    println!(
        "shellglass: mirroring {} at http://{}/",
        describe_source(&source),
        listener.local_addr()?
    );
    let (rx, _notifier) = pty::start(&source.command())?;
    let live = diff::Live::spawn(rx);
    if let Some((l, key)) = ssh_ready {
        let target = ssh::Target::Single(Arc::clone(&live));
        // ponytail: unsupervised — an SSH failure logs and dies; HTTP is unaffected.
        tokio::spawn(async move {
            if let Err(e) = ssh::serve(l, key, target).await {
                eprintln!("shellglass: ssh server error: {e}");
            }
        });
    }
    let state = AppState {
        font_css: Arc::new(render::font_face_css(&s.fonts, "/fonts/")),
        config: s.config,
        resolver: s.resolver,
        fonts: s.fonts,
        template: s.template,
        live,
    };
    axum::serve(listener, server::app(state)).await?;
    Ok(())
}

/// Client mode: render locally and push frames to a remote hub. The PTY (and the
/// command) start only once the hub has accepted a registration — `client::run`
/// invokes the closure after the first successful register.
async fn run_push(url: String, key: String, source: SourceArgs) -> Result<()> {
    let id = proto::session_id(&key);
    let base = url.trim_end_matches('/');
    // Print the view URL before the backend can take the terminal (PTY raw mode).
    println!("shellglass: pushing live to {base}; view at {base}/s/{id}");
    let s = setup(&source)?;
    client::run(
        url,
        key,
        id,
        s.config,
        s.resolver,
        s.fonts,
        s.template,
        || pty::start(&source.command()),
    )
    .await
}

/// One-line description of what a source mirrors, for the startup log.
fn describe_source(source: &SourceArgs) -> String {
    format!("`{}`", source.command().join(" "))
}

/// Serve the hub, terminating TLS per `tls`. Plain HTTP keeps the `SO_REUSEADDR`
/// listener via `axum::serve`; the TLS paths hand the same reuseaddr listener to
/// `axum-server`. ACME drives certificate issuance/renewal on a background task.
async fn serve_hub(
    allowed: std::collections::HashSet<String>,
    addr: &str,
    tls: Tls,
    ssh_bind: Option<String>,
    ssh_host_key: Option<PathBuf>,
) -> Result<()> {
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
    let hub_state = hub::HubState::new(allowed, base);
    // Optional read-only SSH view: the session id is the SSH username. A setup failure
    // must not abort the hub's HTTP service — log and continue without the SSH view.
    if let Some(ssh_addr) = &ssh_bind {
        match prepare_ssh(ssh_addr, ssh_host_key.as_deref(), "<session-id>") {
            Ok((l, key)) => {
                let target = ssh::Target::Hub(hub_state.clone());
                // ponytail: unsupervised — an SSH runtime failure logs and dies; HTTP
                // is unaffected.
                tokio::spawn(async move {
                    if let Err(e) = ssh::serve(l, key, target).await {
                        eprintln!("shellglass: ssh server error: {e}");
                    }
                });
            }
            Err(e) => eprintln!("shellglass: SSH view disabled — {e:#}"),
        }
    }
    // Kept for the SIGTERM path: triggers a WS Close to every pusher so they detect
    // the shutdown at once (see shutdown_signal / graceful).
    let shutdown = hub_state.clone();
    let app = hub::app(hub_state);
    // ConnectInfo::<SocketAddr> so auth-failure logging can record the source IP
    // (fail2ban) — required on every serving path or the extractor 500s.
    let make = || {
        app.clone()
            .into_make_service_with_connect_info::<std::net::SocketAddr>()
    };
    match tls {
        Tls::None => {
            println!("shellglass hub at http://{local}/");
            // On SIGTERM: tell pushers to close, then drop the server — its
            // connections FIN while the container network is still up, so viewers
            // and pushers reconnect instead of black-holing until a TCP timeout.
            tokio::select! {
                r = axum::serve(listener, make()) => r?,
                _ = shutdown_signal() => graceful(&shutdown).await,
            }
        }
        Tls::Static { cert, key } => {
            use axum_server::tls_rustls::RustlsConfig;
            let config = RustlsConfig::from_pem_file(&cert, &key)
                .await
                .with_context(|| format!("loading TLS cert {cert:?} + key {key:?}"))?;
            let std_listener = listener.into_std()?;
            println!("shellglass hub at https://{local}/");
            let handle = axum_server::Handle::new();
            spawn_tls_shutdown(handle.clone(), shutdown.clone());
            axum_server::from_tcp_rustls(std_listener, config)?
                .handle(handle)
                .serve(make())
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
            let handle = axum_server::Handle::new();
            spawn_tls_shutdown(handle.clone(), shutdown.clone());
            axum_server::from_tcp(std_listener)?
                .acceptor(acceptor)
                .handle(handle)
                .serve(make())
                .await?;
        }
    }
    Ok(())
}

/// Resolve when the process is asked to stop: SIGTERM (`docker stop`/`restart`,
/// systemd, k8s) or SIGINT (Ctrl-C).
///
/// Installing these matters most in a container: shellglass runs as PID 1, and the
/// kernel *ignores* any signal with no installed handler for PID 1 — so an unhandled
/// SIGTERM makes `docker stop` wait the full grace period, then SIGKILL, which severs
/// connections as the network namespace is torn down (no FIN reaches clients — they
/// black-hole until a TCP timeout). Handling it lets us close cleanly while the
/// network is still up.
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    let mut int = signal(SignalKind::interrupt()).expect("install SIGINT handler");
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
}

/// Signal every pusher to close (WS Close → prompt reconnect), then give the closes a
/// moment to flush before the caller drops the plain-HTTP server (which FINs the rest).
async fn graceful(hub: &hub::HubState) {
    hub.trigger_shutdown();
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
}

/// TLS-path shutdown: axum-server can't be dropped mid-`serve` like `axum::serve`, so
/// drive its `Handle` — signal pushers, then force-close connections after a short
/// grace (infinite SSE/WS would otherwise never drain).
fn spawn_tls_shutdown(handle: axum_server::Handle<std::net::SocketAddr>, hub: hub::HubState) {
    tokio::spawn(async move {
        shutdown_signal().await;
        hub.trigger_shutdown();
        handle.graceful_shutdown(Some(std::time::Duration::from_millis(500)));
    });
}

/// Bind the SSH listener and resolve its host key (printing the connection hint +
/// fingerprint). Returned as a pair the caller spawns `ssh::serve` on. Fallible so a
/// privileged/in-use port or an unwritable host key disables only the SSH view, never
/// the HTTP service.
fn prepare_ssh(
    addr: &str,
    key_path: Option<&std::path::Path>,
    hint_user: &str,
) -> Result<(tokio::net::TcpListener, russh::keys::PrivateKey)> {
    let l = bind(addr)?;
    let key = ssh::setup(l.local_addr()?, key_path, hint_user)?;
    Ok((l, key))
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

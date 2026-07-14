//! The clap surface + mode runners shared by every binary: the full
//! multi-call `shellglass` CLI and the slim per-mode executables are all thin
//! wrappers over the types here, so flags and behavior can't drift. Modes are
//! cargo features; a subset build simply lacks the other subcommands.

#[cfg(feature = "push")]
use crate::client;
#[cfg(feature = "serve")]
use crate::diff;
#[cfg(feature = "hub")]
use crate::hub;
use crate::proto;
#[cfg(feature = "serve")]
use crate::render;
#[cfg(feature = "serve")]
use crate::server::{self, AppState};
#[cfg(feature = "ssh-view")]
use crate::ssh;
#[cfg(feature = "mirror")]
use crate::{config::Config, fonts, fonts::FontFile, fonts::Resolver, pty};
#[cfg(any(feature = "serve", feature = "push", feature = "hub"))]
use anyhow::Context;
use anyhow::Result;
use clap::Parser;
#[cfg(any(feature = "mirror", feature = "hub"))]
use std::path::PathBuf;
#[cfg(feature = "mirror")]
use std::sync::Arc;

#[derive(Parser, Debug)]
#[command(
    name = "shellglass",
    version,
    about = "Mirror an interactive terminal command as live HTML"
)]
pub struct Cli {
    #[command(subcommand)]
    action: Action,
}

impl Cli {
    /// Dispatch the chosen mode — the single entry point every binary uses.
    ///
    /// # Errors
    /// Whatever the mode returns; the binary reports it and exits non-zero.
    pub async fn run(self) -> Result<()> {
        match self.action {
            Action::GenKey { api, id_salt } => gen_key(api, &id_salt),
            Action::PrintId { key, api, id_salt } => print_id(&key, api, &id_salt),
            #[cfg(feature = "serve")]
            Action::Serve(args) => args.run().await,
            #[cfg(feature = "push")]
            Action::Push(args) => args.run().await,
            #[cfg(feature = "hub")]
            Action::Hub(args) => args.run().await,
            #[cfg(feature = "sessions")]
            Action::Sessions(args) => args.run().await,
        }
    }
}

/// `shellglass sessions` — the hub-control client (see [`crate::apictl`]).
#[cfg(feature = "sessions")]
#[derive(clap::Args, Debug)]
pub struct SessionsArgs {
    /// Hub base URL (e.g. `https://hub.example.com`) — positional, like `push`'s.
    url: String,

    /// Management-API key (or the `SHELLGLASS_API_KEY` env var). Its API id
    /// (`print-id --key K --api`) must be on the hub's `--api-allow`.
    /// (`allow_hyphen_values`: a secret may start with `-`.)
    #[arg(
        long,
        env = "SHELLGLASS_API_KEY",
        hide_env_values = true,
        allow_hyphen_values = true
    )]
    key: String,

    #[command(subcommand)]
    cmd: SessionsCmd,
}

/// The management operations. Removal names its namespace explicitly
/// (`--id` XOR `--slug`) for the same reason the API has two delete routes:
/// an un-aliased slug IS the session id, so a guessing form could delete the
/// wrong thing.
#[cfg(feature = "sessions")]
#[derive(clap::Subcommand, Debug)]
enum SessionsCmd {
    /// List registered sessions (slug, live/offline, per-transport viewer counts,
    /// session id).
    List,
    /// Register a session by its public session id (from `print-id`, never a
    /// key), optionally aliased to a view-URL slug.
    Add {
        /// The session id (64 hex chars).
        id: String,
        /// Public view slug (`/s/<slug>`); defaults to the id.
        #[arg(long)]
        slug: Option<String>,
    },
    /// Remove a session — by exactly one of `--id` or `--slug`.
    #[command(group(clap::ArgGroup::new("target").required(true).multiple(false)))]
    Remove {
        /// Remove by SESSION ID.
        #[arg(long, group = "target")]
        id: Option<String>,
        /// Remove by VIEW SLUG.
        #[arg(long, group = "target")]
        slug: Option<String>,
    },
}

#[cfg(feature = "sessions")]
impl SessionsArgs {
    /// Run the hub-control client (the `sessions` action).
    ///
    /// # Errors
    /// Connection/auth failures and API rejections, with the API's own error
    /// message where it provides one.
    pub async fn run(self) -> Result<()> {
        match &self.cmd {
            SessionsCmd::List => crate::apictl::list(&self.url, &self.key).await,
            SessionsCmd::Add { id, slug } => {
                crate::apictl::add(&self.url, &self.key, id, slug.as_deref()).await
            }
            SessionsCmd::Remove { id: Some(id), .. } => {
                crate::apictl::remove_by_id(&self.url, &self.key, id).await
            }
            SessionsCmd::Remove {
                slug: Some(slug), ..
            } => crate::apictl::remove_by_slug(&self.url, &self.key, slug).await,
            SessionsCmd::Remove { .. } => unreachable!("clap group requires --id or --slug"),
        }
    }
}

/// Each variant is one self-contained mode; clap only accepts the flags that belong
/// to the chosen subcommand, so incompatible options can't be combined by construction.
#[derive(clap::Subcommand, Debug)]
enum Action {
    /// Generate a secure random secret key, print it with its session id, and exit.
    GenKey {
        /// Mint a management-API key instead: print the key with its API id
        /// (for a hub's `--api-allow`). API and session ids live in separate
        /// salt domains — one secret is never a credential for both.
        #[arg(long)]
        api: bool,
        #[command(flatten)]
        id_salt: IdSaltArg,
    },

    /// Print the session id for a key (to add to a hub's `hub --allow`).
    PrintId {
        #[command(flatten)]
        key: KeyArg,
        /// Print the key's API id instead (for a hub's `--api-allow`).
        #[arg(long)]
        api: bool,
        #[command(flatten)]
        id_salt: IdSaltArg,
    },

    /// Mirror a terminal locally: serve the live HTML viewer over HTTP (self-contained).
    #[cfg(feature = "serve")]
    Serve(ServeArgs),

    /// Mirror a terminal and push frames to a remote hub instead of serving locally.
    #[cfg(feature = "push")]
    Push(PushArgs),

    /// Run as a hub: receive pushes from clients and re-serve their sessions.
    #[cfg(feature = "hub")]
    Hub(HubArgs),

    /// Manage a hub's sessions over its management API: list, add, remove.
    #[cfg(feature = "sessions")]
    Sessions(SessionsArgs),
}

/// Args for `serve` (and the `shellglass-serve` binary).
#[cfg(feature = "serve")]
#[derive(clap::Args, Debug)]
pub struct ServeArgs {
    #[command(flatten)]
    source: SourceArgs,

    /// Address to bind the HTTP server.
    #[arg(short, long, default_value = "127.0.0.1:8080")]
    bind: String,

    /// Origin allowed to embed this mirror iframe-lessly from another origin
    /// (sets CORS on the data routes the embed fetches); repeat for several, or
    /// `*` for any. Not needed when the embed is same-origin (e.g. a reverse
    /// proxy mounts this under the host's own domain). Off by default.
    #[arg(long = "cors-origin", value_name = "ORIGIN")]
    cors_origin: Vec<String>,

    /// Also serve a read-only ANSI view over SSH on this address (e.g.
    /// `127.0.0.1:2222`). Any username connects — `ssh -p 2222 x@host`.
    #[arg(long)]
    ssh_bind: Option<String>,

    /// OpenSSH-format host key for the SSH view. Generated + persisted (0600) at
    /// this path on first run; without it, a key under `$XDG_STATE_HOME` is used.
    #[arg(long)]
    ssh_host_key: Option<PathBuf>,

    /// Path to a file shown as a banner (MOTD) to each SSH viewer before the live
    /// view. Displayed VERBATIM — all control characters are preserved (ANSI
    /// colors, cursor moves, art). Off by default.
    #[arg(long, value_name = "PATH")]
    ssh_motd_file: Option<PathBuf>,

    /// Seconds to show the `--ssh-motd-file` banner before the live view starts.
    #[arg(long, value_name = "N", default_value_t = 5)]
    ssh_motd_delay: u64,
}

/// Args for `push` (and the `shellglass-push` binary).
#[cfg(feature = "push")]
#[derive(clap::Args, Debug)]
pub struct PushArgs {
    /// Hub base URL to push to (e.g. `https://hub.example.com`).
    url: String,

    #[command(flatten)]
    key: KeyArg,

    #[command(flatten)]
    source: SourceArgs,
}

/// The terminal source, shared by `serve` and `push` (both render locally).
#[cfg(feature = "mirror")]
#[derive(clap::Args, Debug)]
struct SourceArgs {
    /// Path to a TOML config (fonts + `symbol_map`). Optional; falls back to
    /// $SHELLGLASS_CONFIG.
    #[arg(short, long, env = "SHELLGLASS_CONFIG")]
    config: Option<PathBuf>,

    /// EXPERIMENTAL: on a terminal that renders kitty/iTerm2 graphics but not sixel,
    /// transcode sixel into that protocol (via kitty Unicode placeholders) so
    /// sixel-emitting tools — notably through tmux — show up locally and mirror to
    /// the web. Advertises sixel to the child so it emits it. Off by default.
    #[arg(long)]
    sixel_compat: bool,

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

#[cfg(feature = "mirror")]
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
pub struct KeyArg {
    #[arg(long, env = "SHELLGLASS_KEY", allow_hyphen_values = true)]
    key: String,
}

/// The optional per-system salt extension, shared by every command that
/// derives ids (`hub`, `gen-key`, `print-id`, `push`). One value per id
/// ecosystem: the hub and everyone deriving ids for it must agree, or pushes
/// are rejected (403) and printed view URLs are wrong. Empty (the default) is
/// the un-extended derivation — existing ids stay exactly as they are.
#[derive(clap::Args, Debug, Default)]
pub struct IdSaltArg {
    /// Per-system salt extension mixed into session/API id derivation, so the
    /// same secret yields different ids on differently-salted systems. Not a
    /// secret; set it once and keep it — changing it invalidates every
    /// registered id.
    #[arg(
        long = "id-salt",
        env = "SHELLGLASS_ID_SALT",
        default_value = "",
        hide_default_value = true
    )]
    id_salt: String,
}

#[cfg(feature = "hub")]
#[derive(clap::Args, Debug)]
pub struct HubArgs {
    /// Address to bind the hub's HTTP(S) server.
    #[arg(short, long, default_value = "127.0.0.1:8080")]
    bind: String,

    /// Origin allowed to embed a session iframe-lessly from another origin (sets
    /// CORS on the per-session data routes the embed fetches); repeat, or `*` for
    /// any. Not needed when the embed is same-origin (a reverse proxy mounts the
    /// hub under the host's own domain). Off by default.
    #[arg(long = "cors-origin", value_name = "ORIGIN")]
    cors_origin: Vec<String>,

    /// A session id permitted to push, optionally with a public view-URL slug:
    /// `<id>` or `<id>:<slug>`; repeat for several. The slug is the only way to view
    /// the session (`/s/<slug>`); with no `:slug` it defaults to the id. Pushes whose
    /// key doesn't hash to a listed id get 403. Compute an id with `print-id --key K`.
    #[arg(long = "allow", value_name = "SESSION_ID[:SLUG]")]
    allow: Vec<String>,

    /// An API id permitted to call the session-management API (`/api/sessions`);
    /// repeat for several. Compute one with `print-id --key K --api` (API ids live
    /// in their own salt domain — a session key is never an API credential). With
    /// no --api-allow the whole /api namespace is off (404).
    #[arg(long = "api-allow", value_name = "API_ID")]
    api_allow: Vec<String>,

    /// Persist the session registry here, surviving restarts. When the file
    /// loads, it IS the registry and --allow is ignored (announced at startup);
    /// when it doesn't exist yet, --allow seeds it. Every API change writes it.
    /// Without this flag, runtime changes are memory-only and --allow re-seeds
    /// each start.
    #[arg(long = "sessions-file", value_name = "PATH")]
    sessions_file: Option<PathBuf>,

    /// The ids on `--allow`/`--api-allow` must then come from `gen-key`/
    /// `print-id` run with the SAME `--id-salt`.
    #[command(flatten)]
    id_salt: IdSaltArg,

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
    /// `0.0.0.0:2222`). Connect with the session's view handle — its slug, which
    /// defaults to the session id — as the username: `ssh -p 2222 <slug>@host`.
    #[arg(long)]
    ssh_bind: Option<String>,

    /// OpenSSH-format host key for the SSH view. Generated + persisted (0600) at
    /// this path on first run; without it, a key under `$XDG_STATE_HOME` is used.
    #[arg(long)]
    ssh_host_key: Option<PathBuf>,

    /// Path to a file shown as a banner (MOTD) to each SSH viewer before the live
    /// view. Displayed VERBATIM — all control characters are preserved (ANSI
    /// colors, cursor moves, art). Off by default.
    #[arg(long, value_name = "PATH")]
    ssh_motd_file: Option<PathBuf>,

    /// Seconds to show the `--ssh-motd-file` banner before the live view starts.
    #[arg(long, value_name = "N", default_value_t = 5)]
    ssh_motd_delay: u64,
}

/// How the hub should terminate TLS.
#[cfg(feature = "hub")]
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

#[cfg(feature = "hub")]
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

/// Print the session id — or with `--api`, the API id — for a key (the
/// `print-id` action).
///
/// # Errors
/// Never; `Result` for dispatch uniformity.
pub fn print_id(key: &KeyArg, api: bool, id_salt: &IdSaltArg) -> Result<()> {
    if api {
        println!("{}", proto::api_id_ext(&key.key, &id_salt.id_salt));
    } else {
        println!("{}", proto::session_id_ext(&key.key, &id_salt.id_salt));
    }
    Ok(())
}

#[cfg(feature = "serve")]
impl ServeArgs {
    /// Run the standalone local mirror (the `serve` action).
    ///
    /// # Errors
    /// Config/font/bind failures before the PTY starts; server errors after.
    pub async fn run(self) -> Result<()> {
        run_serve(
            self.source,
            &self.bind,
            self.cors_origin,
            self.ssh_bind,
            self.ssh_host_key,
            self.ssh_motd_file,
            self.ssh_motd_delay,
        )
        .await
    }
}

#[cfg(feature = "push")]
impl PushArgs {
    /// Run the push client (the `push` action).
    ///
    /// # Errors
    /// Config/font failures before registration; the client loop's after.
    pub async fn run(self) -> Result<()> {
        run_push(self.url, self.key.key, self.source).await
    }
}

#[cfg(feature = "hub")]
impl HubArgs {
    /// Run the hub (the `hub` action).
    ///
    /// # Errors
    /// Bad `--allow`/TLS flags or bind failures; server errors after.
    pub async fn run(self) -> Result<()> {
        let tls = Tls::from_args(&self)?;
        let api_allow = hub::parse_api_allow(&self.api_allow).context("parsing --api-allow")?;
        // The registry source of truth: a loadable --sessions-file wins over
        // --allow (announced); a missing one gets seeded FROM --allow; a
        // corrupt one is a hard error (load_sessions), never a silent re-seed.
        let allow = match &self.sessions_file {
            Some(path) => match hub::load_sessions(path)? {
                Some(loaded) => {
                    if !self.allow.is_empty() {
                        eprintln!(
                            "shellglass: --allow ignored — session registry loaded from {}",
                            path.display()
                        );
                    }
                    loaded
                }
                None => {
                    let seed = hub::parse_allow(&self.allow).context("parsing --allow")?;
                    eprintln!(
                        "shellglass: seeding new sessions file {} from --allow",
                        path.display()
                    );
                    seed
                }
            },
            None => hub::parse_allow(&self.allow).context("parsing --allow")?,
        };
        if allow.is_empty() && api_allow.is_empty() {
            eprintln!(
                "shellglass: warning — no sessions registered and no --api-allow; the hub will reject all pushes (403)"
            );
        }
        serve_hub(
            HubSetup {
                allow,
                api_allow,
                id_salt: self.id_salt.id_salt,
                sessions_file: self.sessions_file,
                cors_origins: self.cors_origin,
            },
            &self.bind,
            tls,
            self.ssh_bind,
            self.ssh_host_key,
            self.ssh_motd_file,
            self.ssh_motd_delay,
        )
        .await
    }
}

/// Mint a new secret key (32 bytes of OS randomness, URL-safe base64) and print it
/// with its session id — or, with `--api`, its API id. The key is the write
/// capability (keep it secret); the id is what goes on the hub's `--allow`
/// (sessions) or `--api-allow` (management API).
///
/// # Errors
/// Only if the OS randomness source fails.
pub fn gen_key(api: bool, id_salt: &IdSaltArg) -> Result<()> {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|e| anyhow::anyhow!("reading OS randomness for the new key: {e}"))?;
    let key = base64::Engine::encode(&URL_SAFE_NO_PAD, bytes);
    if api {
        println!("key:    {key}");
        println!("api-id: {}", proto::api_id_ext(&key, &id_salt.id_salt));
    } else {
        println!("key: {key}");
        println!("id:  {}", proto::session_id_ext(&key, &id_salt.id_salt));
    }
    Ok(())
}

/// Config-derived state shared by `serve` and `push`: fonts resolved and read,
/// template loaded. Deliberately does NOT start the PTY — `push` must register
/// with the hub first, so a bad hub address or a down hub is reported (and
/// retried) before the terminal is switched to raw mode and the command runs.
#[cfg(feature = "mirror")]
struct Setup {
    config: Arc<Config>,
    resolver: Arc<Resolver>,
    fonts: Arc<Vec<FontFile>>,
    template: Arc<String>,
}

#[cfg(feature = "mirror")]
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
    // kitty's box model: the cell derives from the font. An explicit
    // `line_height` in the config still wins; otherwise the served font's own
    // metrics set it, so descenders and accents fit the row by construction
    // (line_height_px falls back to 1.2 only when no font file resolved).
    if config.line_height.is_none() {
        config.line_height = fonts::metric_line_height(&fonts);
    }
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
#[cfg(feature = "serve")]
async fn run_serve(
    source: SourceArgs,
    bind_addr: &str,
    cors_origins: Vec<String>,
    ssh_bind: Option<String>,
    ssh_host_key: Option<PathBuf>,
    ssh_motd_file: Option<PathBuf>,
    ssh_motd_delay: u64,
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
    let (rx, _notifier) = pty::start(&source.command(), source.sixel_compat)?;
    // Feed the image store from a sibling subscription: every frame's payloads
    // are inserted (no-ops when already present), with the frame's own
    // placements protected from the byte-cap eviction.
    let images = Arc::new(std::sync::Mutex::new(diff::ImageStore::new(
        64 * 1024 * 1024,
    )));
    {
        let images = Arc::clone(&images);
        let mut rx = rx.clone();
        tokio::spawn(async move {
            loop {
                if let crate::model::Frame::Screen(g) = &**rx.borrow_and_update()
                    && !g.image_data.is_empty()
                {
                    let protected: std::collections::HashSet<String> =
                        g.images.iter().map(|p| p.hash.clone()).collect();
                    let mut store = images.lock().unwrap();
                    for (hash, blob) in &g.image_data {
                        store.insert(
                            hash.clone(),
                            blob.mime.clone(),
                            blob.bytes.clone(),
                            &protected,
                        );
                    }
                }
                if rx.changed().await.is_err() {
                    return; // PTY gone; the server is shutting down with it
                }
            }
        });
    }
    let live = diff::Live::spawn(rx);
    if let Some((l, key)) = ssh_ready {
        let target = ssh::Target::Single(Arc::clone(&live));
        let motd = load_ssh_motd(ssh_motd_file.as_deref(), ssh_motd_delay);
        // ponytail: unsupervised — an SSH failure logs and dies; HTTP is unaffected.
        tokio::spawn(async move {
            if let Err(e) = ssh::serve(l, key, target, motd).await {
                eprintln!("shellglass: ssh server error: {e}");
            }
        });
    }
    // Relative: the page lives at `/`, and relative URLs survive a
    // subpath-mounting reverse proxy that absolute ones can't know about.
    let font_css = render::font_face_css(&s.fonts, "fonts/");
    // Tag the rendering config, so a viewer stays reloadable when serve is
    // restarted with a DIFFERENT config: the process death drops the SSE, the
    // browser reconnects on the same (now stale) page, and the new tag differing
    // from the one it first saw triggers a re-fetch. Same config across a restart
    // hashes identical, so an ordinary restart reloads no one.
    let cfg_json = render::render_config_json(&s.config, &s.resolver);
    live.set_reload_tag(&crate::proto::config_tag(&[
        &font_css,
        &cfg_json,
        &s.template,
    ]));
    let state = AppState {
        font_css: Arc::new(font_css),
        config: s.config,
        resolver: s.resolver,
        fonts: s.fonts,
        template: s.template,
        live,
        images,
    };
    axum::serve(listener, server::app_with_cors(state, &cors_origins)).await?;
    Ok(())
}

/// Client mode: render locally and push frames to a remote hub. The PTY (and the
/// command) start only once the hub has accepted a registration — `client::run`
/// invokes the closure after the first successful register.
#[cfg(feature = "push")]
async fn run_push(url: String, key: String, source: SourceArgs) -> Result<()> {
    // The client derives NO ids: authorization sends the key itself, font URLs
    // in the pushed CSS are page-relative, and the authoritative view URL is
    // announced in the HUB's log on connect — the client could only guess it
    // (it knows neither the slug nor the hub's --id-salt).
    let base = url.trim_end_matches('/');
    println!("shellglass: pushing live to {base}");
    let s = setup(&source)?;
    let sixel_compat = source.sixel_compat;
    client::run(url, key, s.config, s.resolver, s.fonts, s.template, || {
        pty::start(&source.command(), sixel_compat)
    })
    .await
}

/// One-line description of what a source mirrors, for the startup log.
#[cfg(feature = "serve")]
fn describe_source(source: &SourceArgs) -> String {
    format!("`{}`", source.command().join(" "))
}

/// The hub's authorization/registry setup, grouped out of `serve_hub`'s
/// signature: everything `HubState` is built from except the base URL (which
/// depends on the TLS mode resolved inside).
#[cfg(feature = "hub")]
struct HubSetup {
    allow: hub::AllowConfig,
    api_allow: std::collections::HashSet<String>,
    id_salt: String,
    sessions_file: Option<PathBuf>,
    cors_origins: Vec<String>,
}

/// Serve the hub, terminating TLS per `tls`. Plain HTTP keeps the `SO_REUSEADDR`
/// listener via `axum::serve`; the TLS paths hand the same reuseaddr listener to
/// `axum-server`. ACME drives certificate issuance/renewal on a background task.
#[cfg(feature = "hub")]
async fn serve_hub(
    setup: HubSetup,
    addr: &str,
    tls: Tls,
    ssh_bind: Option<String>,
    ssh_host_key: Option<PathBuf>,
    ssh_motd_file: Option<PathBuf>,
    ssh_motd_delay: u64,
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
    let mut hub_state = hub::HubState::new(setup.allow, base)
        .with_api_allowed(setup.api_allow)
        .with_id_salt(setup.id_salt);
    if let Some(path) = setup.sessions_file {
        hub_state = hub_state.with_persistence(path);
        // Materialize the seed (file absent) / normalize the loaded file now,
        // so a crash before the first API call still leaves a valid store.
        hub_state.persist().context("writing the sessions file")?;
    }
    // Optional read-only SSH view: the view handle (the slug; = the id when
    // un-aliased) is the SSH username. A setup failure must not abort the hub's
    // HTTP service — log and continue without the SSH view.
    if let Some(ssh_addr) = &ssh_bind {
        match prepare_ssh(ssh_addr, ssh_host_key.as_deref(), "<slug>") {
            Ok((l, key)) => {
                let target = ssh::Target::Hub(hub_state.clone());
                let motd = load_ssh_motd(ssh_motd_file.as_deref(), ssh_motd_delay);
                // ponytail: unsupervised — an SSH runtime failure logs and dies; HTTP
                // is unaffected.
                tokio::spawn(async move {
                    if let Err(e) = ssh::serve(l, key, target, motd).await {
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
    let app = hub::app_with_cors(hub_state, &setup.cors_origins);
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
#[cfg(feature = "hub")]
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
#[cfg(feature = "hub")]
async fn graceful(hub: &hub::HubState) {
    hub.trigger_shutdown();
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
}

/// TLS-path shutdown: axum-server can't be dropped mid-`serve` like `axum::serve`, so
/// drive its `Handle` — signal pushers, then force-close connections after a short
/// grace (infinite SSE/WS would otherwise never drain).
#[cfg(feature = "hub")]
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
#[cfg(feature = "ssh-view")]
fn prepare_ssh(
    addr: &str,
    key_path: Option<&std::path::Path>,
    hint_user: &str,
) -> Result<(tokio::net::TcpListener, russh::keys::PrivateKey)> {
    let l = bind(addr)?;
    let key = ssh::setup(l.local_addr()?, key_path, hint_user)?;
    Ok((l, key))
}

/// Build the SSH MOTD from `--ssh-motd-file`, or `None` if unset. A read failure
/// logs and disables only the banner — the SSH view still runs — consistent with how
/// a bad host key disables just the SSH view, not the whole mirror.
#[cfg(feature = "ssh-view")]
fn load_ssh_motd(path: Option<&std::path::Path>, delay_secs: u64) -> Option<ssh::Motd> {
    match ssh::Motd::load(path?, delay_secs) {
        Ok(m) => Some(m),
        Err(e) => {
            eprintln!("shellglass: SSH MOTD disabled — {e:#}");
            None
        }
    }
}

/// Bind with `SO_REUSEADDR` so a hub restart can rebind immediately — otherwise the
/// previous run's client/browser connections linger in `TIME_WAIT` and the fresh
/// bind fails with "address in use" for up to a minute.
#[cfg(any(feature = "serve", feature = "hub", feature = "ssh-view"))]
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

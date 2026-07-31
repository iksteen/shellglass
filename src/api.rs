//! Stable embedding surface for parser-independent frame producers.
//!
//! External producers supply a [`SourceSession`];
//! shellglass retains ownership of presentation setup, diffing, HTTP/SSE,
//! recording, SSH, and hub push behavior. The CLI delegates to this module so
//! library and command-line integrations cannot drift.

use crate::config::Config;
use crate::fonts::{self, FontFile, Resolver};
pub use crate::source::{FramePublisher, SourceSession, external_source};
use anyhow::{Context, Result};
use std::path::Path;
#[cfg(feature = "serve-api")]
use std::path::PathBuf;
use std::sync::Arc;

/// Config-derived viewer presentation shared by standalone serve and push.
pub struct Presentation {
    pub(crate) config: Arc<Config>,
    pub(crate) resolver: Arc<Resolver>,
    pub(crate) fonts: Arc<Vec<FontFile>>,
    pub(crate) template: Arc<String>,
}

impl Presentation {
    /// Load optional TOML configuration and resolve the complete browser font
    /// and template bundle. `None` uses shellglass defaults.
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let config = match path {
            Some(path) => Config::load(path)?,
            None => Config::default(),
        };
        Self::from_config(config)
    }

    /// Prepare a presentation from an already-parsed configuration.
    pub fn from_config(mut config: Config) -> Result<Self> {
        let resolver = Arc::new(Resolver::build(&config).context("building font resolver")?);
        fonts::resolve_generics(&mut config);
        let fonts = Arc::new(fonts::collect_fonts(&config));
        if config.line_height.is_none() {
            config.line_height = fonts::metric_line_height(&fonts);
        }
        let template = Arc::new(config.template_html().context("loading viewer template")?);
        Ok(Self {
            config: Arc::new(config),
            resolver,
            fonts,
            template,
        })
    }
}

/// Standalone HTTP/SSE publishing options.
///
/// `non_exhaustive`: these mirror the CLI's flags and gain a field whenever one
/// is added, so build them with [`ServeOptions::new`] and assign what you need —
/// a struct literal here would break on every new option.
#[cfg(feature = "serve-api")]
#[non_exhaustive]
pub struct ServeOptions {
    pub bind: String,
    pub cors_origins: Vec<String>,
    pub ssh_bind: Option<String>,
    pub ssh_host_key: Option<PathBuf>,
    pub ssh_motd_file: Option<PathBuf>,
    pub ssh_motd_delay: u64,
    pub record_dir: Option<PathBuf>,
    /// Human-readable producer description used only in the startup line.
    pub source_label: String,
}

#[cfg(feature = "serve-api")]
impl ServeOptions {
    pub fn new(bind: impl Into<String>) -> Self {
        Self {
            bind: bind.into(),
            cors_origins: Vec::new(),
            ssh_bind: None,
            ssh_host_key: None,
            ssh_motd_file: None,
            ssh_motd_delay: 5,
            record_dir: None,
            source_label: "an external frame source".into(),
        }
    }
}

/// Serve any parser-independent source through shellglass's stock viewer.
///
/// Runs until the process ends; use [`serve_with_shutdown`] to stop it on a
/// signal of your own.
#[cfg(feature = "serve-api")]
pub async fn serve<F>(start: F, presentation: Presentation, options: ServeOptions) -> Result<()>
where
    F: FnOnce() -> Result<SourceSession>,
{
    serve_with_shutdown(start, presentation, options, std::future::pending()).await
}

/// [`serve`], stopping gracefully when `shutdown` resolves. Long-lived SSE and
/// SSH viewers are closed, source-forwarding tasks stop, and an active recording
/// is flushed before this returns. An embedder that owns the process lifetime (a
/// GUI, a test, a supervisor) needs this rather than the run-forever default.
#[cfg(feature = "serve-api")]
pub async fn serve_with_shutdown<F, S>(
    start: F,
    presentation: Presentation,
    options: ServeOptions,
    shutdown: S,
) -> Result<()>
where
    F: FnOnce() -> Result<SourceSession>,
    S: std::future::Future<Output = ()> + Send + 'static,
{
    use crate::diff;
    use crate::render;
    use crate::server::{self, AppState};

    let listener = crate::bind(&options.bind)?;
    if let Some(dir) = &options.record_dir {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating record directory {}", dir.display()))?;
    }
    let ssh_ready = match &options.ssh_bind {
        Some(addr) => match crate::ssh::prepare(addr, options.ssh_host_key.as_deref(), "x") {
            Ok(ready) => Some(ready),
            Err(error) => {
                eprintln!("shellglass: SSH view disabled — {error:#}");
                None
            }
        },
        None => None,
    };
    println!(
        "shellglass: mirroring {} at http://{}/",
        options.source_label,
        listener.local_addr()?
    );
    let source = start()?;
    let rx = source.frames.clone();
    let images = Arc::new(std::sync::Mutex::new(diff::ImageStore::new(
        64 * 1024 * 1024,
    )));
    let image_task = {
        let images = Arc::clone(&images);
        let mut rx = rx.clone();
        tokio::spawn(async move {
            loop {
                if let crate::model::Frame::Screen(grid) = &**rx.borrow_and_update()
                    && !grid.image_data.is_empty()
                {
                    let protected: std::collections::HashSet<String> = grid
                        .images
                        .iter()
                        .map(|placement| placement.hash.clone())
                        .collect();
                    let mut store = images.lock().unwrap();
                    for (hash, blob) in &grid.image_data {
                        store.insert(
                            hash.clone(),
                            blob.mime.clone(),
                            blob.bytes.clone(),
                            &protected,
                        );
                    }
                }
                if rx.changed().await.is_err() {
                    return;
                }
            }
        })
    };
    let (live, frame_task) = diff::Live::spawn_managed(rx);
    let ssh_task = ssh_ready.map(|(listener, key)| {
        let target = crate::ssh::Target::Single(Arc::clone(&live));
        let motd = crate::ssh::load_motd(options.ssh_motd_file.as_deref(), options.ssh_motd_delay);
        tokio::spawn(async move {
            if let Err(error) = crate::ssh::serve(listener, key, target, motd).await {
                eprintln!("shellglass: ssh server error: {error}");
            }
        })
    });
    let font_css = render::font_face_css(&presentation.fonts, "fonts/");
    let cfg_json = render::render_config_json(&presentation.config, &presentation.resolver);
    live.set_reload_tag(&crate::proto::config_tag(&[
        &font_css,
        &cfg_json,
        &presentation.template,
    ]));
    let recording = options.record_dir.map(|dir| {
        let (recorder, writer_task) = crate::record::start(dir);
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let live = Arc::clone(&live);
        let config = Arc::clone(&presentation.config);
        let resolver = Arc::clone(&presentation.resolver);
        let template = Arc::clone(&presentation.template);
        let fonts = Arc::clone(&presentation.fonts);
        let record_task = tokio::spawn(async move {
            let register = tokio::task::spawn_blocking(move || {
                let hub_fonts = crate::fonts::hub_font_bundle(&fonts);
                serde_json::to_string(&crate::proto::RegisterBody {
                    // A recording speaks the push contract: the hub/player
                    // reconstructs font CSS from the structured font assets.
                    css: render::head_css_with_font_aliases("", &config, &hub_fonts.aliases),
                    render_cfg: render::render_config_json_with_font_aliases(
                        &config,
                        &resolver,
                        &hub_fonts.aliases,
                    ),
                    template: (*template).clone(),
                    fonts: hub_fonts.assets,
                    no_record: false,
                })
                .expect("register body serializes")
            })
            .await
            .expect("register build task");
            crate::record::record_live(live, register, recorder, stopped).await;
        });
        (stop, record_task, writer_task)
    });
    let state = AppState {
        font_css: Arc::new(font_css),
        config: presentation.config,
        resolver: presentation.resolver,
        fonts: presentation.fonts,
        template: presentation.template,
        live: Arc::clone(&live),
        images,
    };
    let shutdown = async move {
        shutdown.await;
        live.close_viewers();
    };
    let result = axum::serve(
        listener,
        server::app_with_cors(state, &options.cors_origins),
    )
    .with_graceful_shutdown(shutdown)
    .await;

    // Unlike the stock CLI (whose PTY exits the process), an embedder may keep
    // this runtime alive and start another server. Do not leave source or SSH
    // tasks — or an unflushed recording — behind after graceful shutdown.
    image_task.abort();
    frame_task.abort();
    let _ = image_task.await;
    let _ = frame_task.await;
    if let Some(task) = ssh_task {
        task.abort();
        let _ = task.await;
    }
    if let Some((stop, record_task, writer_task)) = recording {
        let _ = stop.send(());
        let _ = record_task.await;
        let _ = writer_task.await;
    }

    result?;
    Ok(())
}

/// Remote hub publishing options.
///
/// `non_exhaustive` for the same reason as [`ServeOptions`] — construct with
/// [`PushOptions::new`].
#[cfg(feature = "push-api")]
#[non_exhaustive]
pub struct PushOptions {
    pub url: String,
    pub key: String,
    pub no_record: bool,
}

#[cfg(feature = "push-api")]
impl PushOptions {
    pub fn new(url: impl Into<String>, key: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            key: key.into(),
            no_record: false,
        }
    }
}

/// Push any parser-independent source to an ordinary shellglass hub.
///
/// The source factory is deliberately invoked only after the hub accepts the
/// authenticated WebSocket upgrade.
#[cfg(feature = "push-api")]
pub async fn push<F>(start: F, presentation: Presentation, options: PushOptions) -> Result<()>
where
    F: FnOnce() -> Result<SourceSession>,
{
    crate::client::run(
        options.url,
        options.key,
        presentation.config,
        presentation.resolver,
        presentation.fonts,
        presentation.template,
        options.no_record,
        start,
    )
    .await
}

#[cfg(all(test, feature = "serve-api", feature = "push-api"))]
mod tests {
    use super::*;
    use crate::model::{Color, Frame, Grid, StyledCell};
    use crate::source::external_source;

    fn frame(text: &str) -> Frame {
        Frame::Screen(Grid {
            source_epoch: 0,
            cols: 1,
            rows: vec![vec![StyledCell {
                text: text.into(),
                ..Default::default()
            }]],
            cursor: None,
            cursor_style: 0,
            default_colors: (Color::Default, Color::Default),
            title: "library source".into(),
            links: Default::default(),
            images: Vec::new(),
            image_data: Default::default(),
        })
    }

    #[tokio::test]
    async fn external_source_serves_stock_snapshot_and_sse() {
        let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = reservation.local_addr().unwrap();
        drop(reservation);
        let ssh_reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let ssh_addr = ssh_reservation.local_addr().unwrap();
        drop(ssh_reservation);
        let key_path = std::env::temp_dir().join(format!(
            "shellglass-api-test-{}-{}.key",
            std::process::id(),
            ssh_addr.port()
        ));
        let _ = std::fs::remove_file(&key_path);

        let (_publisher, source) = external_source(frame("LIBRARY_API_MARKER"));
        let presentation = Presentation::from_config(Config::default()).unwrap();
        let mut options = ServeOptions::new(addr.to_string());
        options.ssh_bind = Some(ssh_addr.to_string());
        options.ssh_host_key = Some(key_path.clone());
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(serve_with_shutdown(
            move || Ok(source),
            presentation,
            options,
            async move {
                let _ = stopped.await;
            },
        ));
        let url = format!("http://{addr}/snapshot");
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let body = loop {
            if let Ok(response) = reqwest::get(&url).await
                && let Ok(body) = response.text().await
                && body.contains("LIBRARY_API_MARKER")
            {
                break body;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "library server did not publish snapshot"
            );
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        };
        assert!(body.contains("library source"));

        use futures_util::StreamExt;
        let response = reqwest::get(format!("http://{addr}/events")).await.unwrap();
        let mut stream = response.bytes_stream();
        let mut events = Vec::new();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while !String::from_utf8_lossy(&events).contains("LIBRARY_API_MARKER") {
            assert!(
                tokio::time::Instant::now() < deadline,
                "library SSE did not publish initial full frame"
            );
            let chunk = tokio::time::timeout(std::time::Duration::from_secs(1), stream.next())
                .await
                .ok()
                .flatten()
                .transpose()
                .unwrap()
                .expect("SSE stream ended before its full frame");
            events.extend_from_slice(&chunk);
        }

        // Keep the infinite SSE response open: shutdown must close it rather
        // than waiting forever for the viewer to disconnect first.
        let _ = stop.send(());
        tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .expect("serve_with_shutdown waited on its SSE viewer")
            .unwrap()
            .unwrap();
        drop(stream);

        // Graceful shutdown is an embedding boundary, not process exit: every
        // auxiliary listener must be gone before the future returns.
        let rebound = std::net::TcpListener::bind(ssh_addr)
            .expect("SSH listener survived serve_with_shutdown");
        drop(rebound);
        let _ = std::fs::remove_file(key_path);
    }

    #[tokio::test]
    async fn external_push_waits_for_upgrade_then_sends_stock_wire() {
        use axum::extract::ws::WebSocketUpgrade;
        use axum::routing::get;
        use std::sync::atomic::{AtomicBool, Ordering};

        let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = reservation.local_addr().unwrap();
        drop(reservation);

        let (publisher, source) = external_source(frame("PUSH_LIBRARY_MARKER"));
        let started = std::sync::Arc::new(AtomicBool::new(false));
        let started_in_factory = started.clone();
        let presentation = Presentation::from_config(Config::default()).unwrap();
        let push_task = tokio::spawn(push(
            move || {
                started_in_factory.store(true, Ordering::SeqCst);
                Ok(source)
            },
            presentation,
            PushOptions::new(format!("http://{addr}"), "test-key"),
        ));

        // Connection refusal is retryable, but must not start the producer.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(!started.load(Ordering::SeqCst));

        let (messages_tx, messages_rx) = tokio::sync::oneshot::channel();
        let sender = std::sync::Arc::new(std::sync::Mutex::new(Some(messages_tx)));
        let app = axum::Router::new().route(
            "/push",
            get({
                let sender = sender.clone();
                move |upgrade: WebSocketUpgrade| {
                    let sender = sender.clone();
                    async move {
                        upgrade.on_upgrade(move |mut socket| async move {
                            let mut messages = Vec::new();
                            while messages.len() < 2 {
                                match socket.recv().await {
                                    Some(Ok(axum::extract::ws::Message::Text(text))) => {
                                        messages.push(text.to_string());
                                    }
                                    Some(Ok(_)) => {}
                                    _ => break,
                                }
                            }
                            if let Some(tx) = sender.lock().unwrap().take() {
                                let _ = tx.send(messages);
                            }
                            while socket.recv().await.is_some() {}
                        })
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        let hub_task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let messages = tokio::time::timeout(std::time::Duration::from_secs(5), messages_rx)
            .await
            .expect("push did not connect to synthetic hub")
            .unwrap();
        assert!(started.load(Ordering::SeqCst));
        assert_eq!(messages.len(), 2);
        assert!(messages[0].contains("\"css\""), "register: {}", messages[0]);
        assert!(
            messages[1].contains("PUSH_LIBRARY_MARKER"),
            "full frame: {}",
            messages[1]
        );

        // Closing the sole publisher closes the external source and lets push end.
        drop(publisher);
        tokio::time::timeout(std::time::Duration::from_secs(5), push_task)
            .await
            .expect("push did not stop after external producer closed")
            .unwrap()
            .unwrap();
        hub_task.abort();
    }
}

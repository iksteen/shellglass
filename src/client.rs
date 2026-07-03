//! Push client: run the live control-mode pipeline locally, but stream the
//! rendered fragments to a remote hub instead of serving them.
//!
//! It registers the page CSS once (`/register`), then opens a single long-lived
//! `/stream` POST and writes length-prefixed frames into its body as the live
//! task produces them. Because frames flow over one persistent connection, the
//! client never blocks on a per-frame HTTP round-trip — that round-trip is what
//! made a remote hub feel laggy. If the connection drops (hub restart, network
//! blip) it re-registers and reopens.

use crate::config::Config;
use crate::fonts::{self, FontFile};
use crate::proto::{KEY_HEADER, RegisterBody, frame_encode};
use crate::render;
use anyhow::{Context, Result, bail};
use bytes::Bytes;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::sync::watch;
use tokio_stream::wrappers::ReceiverStream;

// ponytail: 8 positional args, one call site — an args struct would be ceremony
// for no reader benefit. Bundle them if a second caller ever appears.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    base_url: String,
    key: String,
    id: String,
    config: Arc<Config>,
    fonts: Arc<Vec<FontFile>>,
    template: Arc<String>,
    mut rx: watch::Receiver<String>,
    notifier: Option<crate::pty::Notifier>,
) -> Result<()> {
    let base = base_url.trim_end_matches('/').to_string();
    let http = reqwest::Client::new();
    // The view URL is printed by `main` before any backend (and any PTY raw mode)
    // starts. The hub serves our fonts under this session's id, so bake that URL
    // prefix into the @font-face CSS and upload the font bytes alongside it.
    let font_css = render::font_face_css(&fonts, &format!("/s/{id}/fonts/"));
    let css = render::head_css(&font_css, &config);
    let reg = RegisterBody {
        css,
        template: (*template).clone(),
        fonts: fonts::font_assets(&fonts),
    };
    let reg_body = Bytes::from(serde_json::to_vec(&reg).context("encoding register payload")?);

    let mut first = true;
    // Whether we've reported the hub as unreachable (so we report down/up once per
    // outage, not every retry). In PTY mode this drives a clean pause/restore of the
    // terminal via the notifier; otherwise it's a plain stderr log.
    let mut down = false;
    loop {
        // Register (also re-registers the CSS after a hub restart) before opening
        // the stream. register is a clean request/response that fails fast when the
        // hub is down, so the reconnect loop spins here — cheaply — until it's back,
        // instead of hanging on a stream POST to an unreachable hub.
        match register(&http, &base, &key, &reg_body).await {
            Reg::Ok => {
                first = false;
                if down {
                    report_up(&notifier);
                    down = false;
                }
            }
            Reg::Forbidden => bail!(
                "hub rejected this key: register its session id on the hub \
                 (run `print-id --key <secret>`, add it to the hub's --allow)"
            ),
            Reg::Unreachable(e) if first => return Err(e).context("registering with hub"),
            Reg::Rejected(s) if first => bail!("hub rejected the registration (HTTP {s})"),
            // Retry either way, but say which it is — an HTTP response means the hub
            // is reachable, so don't call it "unreachable".
            Reg::Unreachable(e) => {
                if !down {
                    report_down(&notifier, &format!("hub unreachable ({e}); retrying"));
                    down = true;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
                continue;
            }
            Reg::Rejected(s) => {
                if !down {
                    report_down(
                        &notifier,
                        &format!("hub rejected the request (HTTP {s}); retrying"),
                    );
                    down = true;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
                continue;
            }
        }

        match stream_push(&http, &base, &key, &mut rx).await {
            End::LiveDone => break, // tmux/control task ended — nothing left to push
            End::Disconnected => {
                // Transient — let the next register decide if it's a real outage, so
                // a quick reconnect doesn't flash a pause in PTY mode.
                if notifier.is_none() {
                    eprintln!("shellglass: push connection dropped; reconnecting");
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
    Ok(())
}

/// Report the hub as unreachable: pause+announce in the terminal (PTY mode) or log.
fn report_down(notifier: &Option<crate::pty::Notifier>, msg: &str) {
    match notifier {
        Some(n) => n.hub_down(msg),
        None => eprintln!("shellglass: {msg}"),
    }
}

/// Report the hub as reachable again: restore the terminal (PTY mode) or stay quiet.
fn report_up(notifier: &Option<crate::pty::Notifier>) {
    if let Some(n) = notifier {
        n.hub_up();
    }
}

enum End {
    LiveDone,
    Disconnected,
}

/// Open one streaming POST and pump frames until either the live task ends or the
/// connection breaks.
async fn stream_push(
    http: &reqwest::Client,
    base: &str,
    key: &str,
    rx: &mut watch::Receiver<String>,
) -> End {
    let (tx, body_rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(4);
    let body = reqwest::Body::wrap_stream(ReceiverStream::new(body_rx));
    let request = http
        .post(format!("{base}/stream"))
        .header(KEY_HEADER, key)
        .body(body)
        .send();

    // Feeder: send the current frame immediately, then every subsequent change.
    // A bounded channel + the watch's latest-only semantics give backpressure: if
    // the network stalls, we hold and coalesce, always sending the freshest frame.
    let feeder = async {
        let cur = rx.borrow_and_update().clone();
        if tx.send(Ok(Bytes::from(frame_encode(&cur)))).await.is_err() {
            return false; // body consumer gone (connection dropped)
        }
        loop {
            if rx.changed().await.is_err() {
                return true; // live task ended
            }
            let f = rx.borrow_and_update().clone();
            if tx.send(Ok(Bytes::from(frame_encode(&f)))).await.is_err() {
                return false;
            }
        }
    };

    tokio::select! {
        // The request future resolves only when the body ends or the connection
        // breaks (the hub responds after reading the stream) — treat as a drop.
        _ = request => End::Disconnected,
        live_done = feeder => if live_done { End::LiveDone } else { End::Disconnected },
    }
}

/// Outcome of a register attempt. `Forbidden` is always fatal; the two failure
/// kinds are fatal only at startup (otherwise a transient condition to retry).
/// They're split so we word them honestly: `Unreachable` means no HTTP response
/// (hub down / network), `Rejected` means the hub answered with a bad status —
/// cause left unstated (could be version skew, a proxy, a transient hub error).
enum Reg {
    Ok,
    Forbidden,
    Unreachable(anyhow::Error),
    Rejected(u16),
}

async fn register(http: &reqwest::Client, base: &str, key: &str, body: &Bytes) -> Reg {
    let resp = match http
        .post(format!("{base}/register"))
        .header(KEY_HEADER, key)
        .header("content-type", "application/json")
        .body(body.clone())
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return Reg::Unreachable(e.into()),
    };
    match resp.status().as_u16() {
        200..=299 => Reg::Ok,
        403 => Reg::Forbidden,
        s => Reg::Rejected(s),
    }
}

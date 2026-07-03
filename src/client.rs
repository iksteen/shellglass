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
use crate::fonts::Resolver;
use crate::proto::{frame_encode, session_id, KEY_HEADER};
use crate::{live, render};
use anyhow::{bail, Context, Result};
use bytes::Bytes;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::sync::watch;
use tokio_stream::wrappers::ReceiverStream;

pub async fn run(
    base_url: String,
    key: String,
    target: Option<String>,
    config: Arc<Config>,
    resolver: Arc<Resolver>,
    font_css: String,
) -> Result<()> {
    let base = base_url.trim_end_matches('/').to_string();
    let http = reqwest::Client::new();
    let css = render::head_css(&font_css, &config);
    let id = session_id(&key);
    println!("tmuxsnitch: pushing live to {base}; view at {base}/s/{id}");

    let mut rx = live::start(target, config, resolver);
    let mut first = true;
    loop {
        // Register (also re-registers the CSS after a hub restart) before opening
        // the stream. register is a clean request/response that fails fast when the
        // hub is down, so the reconnect loop spins here — cheaply — until it's back,
        // instead of hanging on a stream POST to an unreachable hub.
        match register(&http, &base, &key, &css).await {
            Reg::Ok => first = false,
            Reg::Forbidden => bail!(
                "hub rejected this key: register its session id on the hub \
                 (run `--key <secret> --print-id`, add it to the hub's --allow)"
            ),
            Reg::Failed(e) if first => return Err(e).context("registering with hub"),
            Reg::Failed(e) => {
                eprintln!("tmuxsnitch: hub unreachable ({e}); retrying");
                tokio::time::sleep(Duration::from_millis(500)).await;
                continue;
            }
        }

        match stream_push(&http, &base, &key, &mut rx).await {
            End::LiveDone => break, // tmux/control task ended — nothing left to push
            End::Disconnected => {
                eprintln!("tmuxsnitch: push connection dropped; reconnecting");
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
    Ok(())
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

/// Outcome of a register attempt: `Forbidden` is always fatal, `Failed` is fatal
/// only at startup (otherwise it's a transient hub-down condition to retry).
enum Reg {
    Ok,
    Forbidden,
    Failed(anyhow::Error),
}

async fn register(http: &reqwest::Client, base: &str, key: &str, css: &str) -> Reg {
    let resp = match http
        .post(format!("{base}/register"))
        .header(KEY_HEADER, key)
        .body(css.to_string())
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return Reg::Failed(e.into()),
    };
    match resp.status().as_u16() {
        200..=299 => Reg::Ok,
        403 => Reg::Forbidden,
        s => Reg::Failed(anyhow::anyhow!("hub register failed: HTTP {s}")),
    }
}

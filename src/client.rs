//! Push client: run the live PTY pipeline locally, but stream its frames to a
//! remote hub over one WebSocket instead of serving them.
//!
//! It opens a single `/push` WebSocket and runs a register-then-stream state
//! machine over it: the first message is a [`RegisterBody`] (page CSS + render
//! config + fonts), then a full picture, then only the deltas against what it
//! already sent — the exact wire messages the hub forwards to its viewers verbatim.
//! The WebSocket is authorized once at the upgrade (a bad key → 403, fatal), so the
//! upgrade succeeding is what gates taking over the terminal: a down or rejecting
//! hub is reported and retried *before* the command runs.
//!
//! Liveness: the client pings every [`PING_INTERVAL`] and treats a run of
//! unanswered pongs — or any send that stalls past [`SEND_TIMEOUT`] — as a dead
//! connection, so a black-holed hub (a `docker kill`/crash that never sends a FIN)
//! is detected in seconds instead of the kernel's ~15-minute retransmission timeout.
//! A clean shutdown (the hub's SIGTERM Close, or a network FIN) is detected at once.
//! On any drop it reconnects with a fresh register + full.

use crate::config::Config;
use crate::diff;
use crate::fonts::{self, FontFile, Resolver};
use crate::model::Frame;
use crate::proto::{KEY_HEADER, MAX_WS_MESSAGE, RegisterBody};
use crate::render;
use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use futures_util::{SinkExt, StreamExt};
use reqwest_websocket::{Bytes, HandshakeError, Message, Upgrade, WebSocket};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

/// How often to ping the hub while connected.
const PING_INTERVAL: Duration = Duration::from_secs(10);
/// Give up (reconnect) after this many pings with no pong in between — a
/// black-holed hub answers none. ~2–3 intervals of slack absorbs a single lost pong.
const MAX_MISSED_PONGS: u32 = 2;
/// A steady-state send (a delta or a ping) that doesn't complete in this long means
/// the connection is wedged (send buffer full against a dead peer) — treat it as a
/// drop rather than block the loop. Backstops the pong heartbeat for active output.
const SEND_TIMEOUT: Duration = Duration::from_secs(15);
/// The first two sends (register + full) can be large — the register carries the
/// font bundle, up to [`MAX_WS_MESSAGE`] — so they get a much longer deadline than a
/// steady-state delta: a big bundle on a slow uplink mustn't false-trip a reconnect
/// before streaming even starts. Still bounds a hub that died right after the upgrade.
const INITIAL_SEND_TIMEOUT: Duration = Duration::from_secs(60);
/// Backoff between reconnect attempts.
const RECONNECT_BACKOFF: Duration = Duration::from_millis(500);

// ponytail: 7 positional args, one call site — an args struct would be ceremony
// for no reader benefit. Bundle them if a second caller ever appears.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    base_url: String,
    key: String,
    config: Arc<Config>,
    resolver: Arc<Resolver>,
    fonts: Arc<Vec<FontFile>>,
    template: Arc<String>,
    // Starts the PTY backend (raw mode, the command itself). Invoked only after the
    // hub has accepted the WebSocket upgrade, so a down or misconfigured hub is
    // reported — and retried — before the command runs and the terminal is taken over.
    start: impl FnOnce() -> Result<(watch::Receiver<Arc<Frame>>, crate::pty::Notifier)>,
) -> Result<()> {
    let base = base_url.trim_end_matches('/').to_string();
    // WebSocket upgrades require HTTP/1.1 (never h2). This client's only HTTP use is
    // the /push upgrade, so force http1.
    let http = reqwest::Client::builder()
        .http1_only()
        .build()
        .context("building HTTP client")?;
    // Font URLs are page-RELATIVE: the hub serves the view at /s/<slug>/ and
    // the fonts under it, so `fonts/<i>` resolves for any slug and behind any
    // subpath mount — the client needs to know nothing about either (SALT v5;
    // the hub stores this CSS verbatim). Upload the font bytes alongside.
    let font_css = render::font_face_css(&fonts, "fonts/");
    let css = render::head_css(&font_css, &config);
    let reg = RegisterBody {
        css,
        template: (*template).clone(),
        render_cfg: render::render_config_json(&config, &resolver),
        fonts: fonts::font_assets(&fonts),
    };
    let reg_json = serde_json::to_string(&reg).context("encoding register payload")?;
    // Fail fast on an over-limit register rather than looping forever: the hub caps a
    // single WS message at MAX_WS_MESSAGE and just closes an oversized one, which the
    // reconnect loop would re-send verbatim. This is the client's own copy of the
    // same limit, so the check is exact.
    if reg_json.len() > MAX_WS_MESSAGE {
        bail!(
            "register payload is {} MiB, over the hub's {} MiB per-message limit — \
             reduce the exported font bundle (fewer or smaller fonts in the config)",
            reg_json.len() / (1024 * 1024),
            MAX_WS_MESSAGE / (1024 * 1024),
        );
    }

    let mut start = Some(start);
    // The PTY backend, started after the first successful upgrade. Until then outage
    // reports go to stderr (the terminal is still ours); after, the notifier
    // pauses/restores the raw session cleanly.
    let mut backend: Option<(watch::Receiver<Arc<Frame>>, crate::pty::Notifier)> = None;
    // Whether we've reported the hub as down (so we report down/up once per outage,
    // not every retry).
    let mut down = false;
    loop {
        let notifier = backend.as_ref().map(|(_, n)| n);
        // Connect (and re-register) before streaming. The upgrade fails fast when the
        // hub is down, so the reconnect loop spins here — cheaply — until it's back.
        // Startup and mid-session failures take the same path; only a rejected key is
        // fatal (retrying can't fix it).
        let ws = match connect(&http, &base, &key).await {
            Ok(ws) => {
                if down {
                    report_up(notifier);
                    down = false;
                }
                ws
            }
            Err(ConnErr::Forbidden) => bail!(
                "hub rejected this key: register its session id on the hub \
                 (run `print-id --key <secret>`, add it to the hub's --allow)"
            ),
            Err(ConnErr::Retry(cause)) => {
                if !down {
                    report_down(notifier, cause);
                    down = true;
                }
                tokio::time::sleep(RECONNECT_BACKOFF).await;
                continue;
            }
        };

        if backend.is_none() {
            // Hub reachable and key accepted — now take the terminal and launch the command.
            backend = Some(start.take().expect("started once")()?);
        }
        let (rx, _) = backend.as_mut().expect("backend started on first Ok");
        match run_session(ws, &reg_json, rx).await {
            End::LiveDone => break, // PTY backend ended — nothing left to push
            End::Disconnected => {
                // Transient — let the next connect decide if it's a real outage, so a
                // quick reconnect doesn't flash a pause in the terminal.
                tokio::time::sleep(RECONNECT_BACKOFF).await;
            }
        }
    }
    Ok(())
}

/// Report the hub as down: pause+announce in the terminal (PTY running) or log to
/// stderr (still ours before the first successful upgrade). The message is built
/// entirely from our own literals plus a `u16` — see [`Cause`]; no peer-supplied
/// text ever reaches the terminal.
fn report_down(notifier: Option<&crate::pty::Notifier>, cause: Cause) {
    let msg = notice(cause);
    match notifier {
        Some(n) => n.hub_down(&msg),
        None => eprintln!("shellglass: {msg}"),
    }
}

/// The operator-facing notice for a down cause — our own literals plus a `u16`, so
/// it is control-char-free by construction (asserted in tests).
fn notice(cause: Cause) -> String {
    match cause {
        Cause::Connect => "hub unreachable (could not connect); retrying".to_string(),
        Cause::Timeout => "hub unreachable (timed out); retrying".to_string(),
        Cause::BadHandshake => "hub unreachable (not a shellglass endpoint?); retrying".to_string(),
        Cause::Rejected(s) => format!("hub rejected the request (HTTP {s}); retrying"),
        Cause::Other => "hub unreachable; retrying".to_string(),
    }
}

/// Report the hub as reachable again: restore the terminal, or stay quiet pre-PTY.
fn report_up(notifier: Option<&crate::pty::Notifier>) {
    if let Some(n) = notifier {
        n.hub_up();
    }
}

enum End {
    LiveDone,
    Disconnected,
}

/// Why a connect attempt failed. `Forbidden` is fatal (retrying can't fix a key the
/// hub doesn't allow); `Retry` is always retried — at startup that means the command
/// doesn't launch until the hub is reachable.
enum ConnErr {
    Forbidden,
    Retry(Cause),
}

/// A fixed classification of a retryable connect failure, derived from the error's
/// KIND — never its free-form text. The connect error can be seeded by the peer (a
/// hub, or a MITM): a botched WebSocket upgrade carries the server's header/protocol
/// values, a TLS failure the certificate fields. We render one of our own literals
/// per variant and never the error's `Display`, so nothing peer-controlled can reach
/// the operator's raw-mode terminal — prevention, not sanitization. `Rejected`'s
/// `u16` is the only interpolated value and a number can't carry a control sequence.
#[derive(Clone, Copy)]
enum Cause {
    /// Couldn't establish the connection (`reqwest::Error::is_connect`) — hub down,
    /// wrong host/port, or a network/TLS failure.
    Connect,
    /// The attempt timed out (`is_timeout`).
    Timeout,
    /// The peer answered but not with a valid WebSocket upgrade — a wrong URL, a
    /// proxy mangling the handshake, or simply not a shellglass `/push` endpoint.
    BadHandshake,
    /// The hub answered the upgrade with a non-101, non-403 status.
    Rejected(u16),
    /// Any other transport failure.
    Other,
}

/// Open the `/push` WebSocket, carrying the secret key in its header.
async fn connect(http: &reqwest::Client, base: &str, key: &str) -> Result<WebSocket, ConnErr> {
    let url = format!("{base}/push");
    let resp = match http
        .get(&url)
        .header(KEY_HEADER, key)
        .upgrade()
        .send()
        .await
    {
        Ok(r) => r,
        // A non-101 status may surface here or in into_websocket() depending on the
        // path — classify handles both.
        Err(e) => return Err(classify(e)),
    };
    match resp.status().as_u16() {
        101 => resp.into_websocket().await.map_err(classify),
        403 => Err(ConnErr::Forbidden),
        s => Err(ConnErr::Retry(Cause::Rejected(s))),
    }
}

/// Map a connect error to a fixed [`Cause`] by inspecting its KIND only — the
/// error's `Display` (which can embed peer-supplied header/protocol/cert text) is
/// never read.
fn classify(e: reqwest_websocket::Error) -> ConnErr {
    use reqwest_websocket::Error as WsErr;
    match e {
        WsErr::Handshake(HandshakeError::UnexpectedStatusCode(code)) => match code.as_u16() {
            403 => ConnErr::Forbidden,
            s => ConnErr::Retry(Cause::Rejected(s)),
        },
        // The peer responded but the upgrade wasn't a valid WebSocket handshake.
        WsErr::Handshake(_) => ConnErr::Retry(Cause::BadHandshake),
        WsErr::Reqwest(re) if re.is_timeout() => ConnErr::Retry(Cause::Timeout),
        WsErr::Reqwest(re) if re.is_connect() => ConnErr::Retry(Cause::Connect),
        _ => ConnErr::Retry(Cause::Other),
    }
}

/// Drive one connected session: register, send a full picture, then stream deltas,
/// pinging for liveness, until the live task ends (`LiveDone`) or the connection
/// breaks/wedges (`Disconnected`).
async fn run_session(
    mut ws: WebSocket,
    reg_json: &str,
    rx: &mut watch::Receiver<Arc<Frame>>,
) -> End {
    // First message is the registration; then the full picture the hub seeds its
    // matrix from (a resize later is a layout change, which encode_delta turns into a
    // fresh full automatically). Both can be large (the register carries fonts), so
    // they get the longer INITIAL_SEND_TIMEOUT.
    if send(
        &mut ws,
        Message::Text(reg_json.to_string()),
        INITIAL_SEND_TIMEOUT,
    )
    .await
    .is_err()
    {
        return End::Disconnected;
    }
    // Content keys of image payloads already uploaded on THIS connection —
    // reset per connect, because a restarted hub lost its stores. Blobs go
    // out before the frame that references them (WS FIFO makes that a hard
    // ordering guarantee for the hub and every viewer behind it).
    let mut sent_blobs = SentBlobs::default();
    let mut prev = rx.borrow_and_update().clone();
    if send_new_blobs(&mut ws, &prev, &mut sent_blobs)
        .await
        .is_err()
    {
        return End::Disconnected;
    }
    if send(
        &mut ws,
        Message::Text(diff::full_message(&prev)),
        INITIAL_SEND_TIMEOUT,
    )
    .await
    .is_err()
    {
        return End::Disconnected;
    }

    let mut ping = tokio::time::interval(PING_INTERVAL);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut missed: u32 = 0;
    loop {
        tokio::select! {
            changed = rx.changed() => {
                if changed.is_err() {
                    return End::LiveDone; // live task ended
                }
                let next = rx.borrow_and_update().clone();
                // A bounded send + the watch's latest-only semantics give backpressure:
                // if the network stalls the delta is computed against the last frame
                // actually sent, coalescing the skipped ones.
                if let Some(msg) = diff::encode_delta(&prev, &next) {
                    if send_new_blobs(&mut ws, &next, &mut sent_blobs).await.is_err() {
                        return End::Disconnected;
                    }
                    if send(&mut ws, Message::Text(msg.to_string()), SEND_TIMEOUT).await.is_err() {
                        return End::Disconnected;
                    }
                }
                prev = next;
            }
            _ = ping.tick() => {
                // Too many pings unanswered → the hub is gone (or black-holed).
                if missed >= MAX_MISSED_PONGS {
                    return End::Disconnected;
                }
                missed += 1;
                if send(&mut ws, Message::Ping(Bytes::new()), SEND_TIMEOUT).await.is_err() {
                    return End::Disconnected;
                }
            }
            msg = ws.next() => match msg {
                Some(Ok(Message::Pong(_))) => missed = 0, // hub alive
                Some(Ok(Message::Close { .. })) => return End::Disconnected, // graceful hub shutdown
                Some(Ok(_)) => {} // text/binary/inbound-ping: nothing to do (read-only push)
                Some(Err(_)) | None => return End::Disconnected, // socket error / closed
            }
        }
    }
}

/// Upload the frame's image payloads this connection hasn't sent yet, as blob
/// messages ahead of the frame itself. The frame's `image_data` covers every
/// on-screen placement, so a full referencing hash H is always preceded (on
/// this FIFO socket) by H's bytes.
/// The hashes uploaded on this connection, FIFO-capped so a long-running
/// animation (a fresh image every frame) can't grow it without bound. Eviction
/// just means a recurring hash is re-sent — harmless, and it also self-heals a
/// hub-side store eviction of the same key.
#[derive(Default)]
struct SentBlobs {
    set: std::collections::HashSet<String>,
    order: std::collections::VecDeque<String>,
}

impl SentBlobs {
    /// Roughly the on-screen image working set plus grace; a hash is 64 bytes.
    const CAP: usize = 4096;

    fn contains(&self, hash: &str) -> bool {
        self.set.contains(hash)
    }

    fn insert(&mut self, hash: String) {
        if self.set.insert(hash.clone()) {
            self.order.push_back(hash);
            while self.order.len() > Self::CAP {
                if let Some(old) = self.order.pop_front() {
                    self.set.remove(&old);
                }
            }
        }
    }
}

async fn send_new_blobs(ws: &mut WebSocket, frame: &Frame, sent: &mut SentBlobs) -> Result<(), ()> {
    let Frame::Screen(grid) = frame;
    for (hash, blob) in &grid.image_data {
        if sent.contains(hash) {
            continue;
        }
        let msg = serde_json::to_string(&crate::proto::BlobMsg {
            blob: crate::proto::BlobBody {
                m: blob.mime.clone(),
                d: B64.encode(&blob.bytes),
            },
        })
        .map_err(|_| ())?;
        // Blobs can be MBs (like the register) — the longer deadline applies.
        send(ws, Message::Text(msg), INITIAL_SEND_TIMEOUT).await?;
        sent.insert(hash.clone());
    }
    Ok(())
}

/// Send one message, treating a stall past `timeout` (send buffer full against a dead
/// peer) as a failure so it can't wedge the session loop. Callers pass
/// [`INITIAL_SEND_TIMEOUT`] for the large register/full and [`SEND_TIMEOUT`] for
/// steady-state deltas/pings.
async fn send(ws: &mut WebSocket, msg: Message, timeout: Duration) -> Result<(), ()> {
    match tokio::time::timeout(timeout, ws.send(msg)).await {
        Ok(Ok(())) => Ok(()),
        _ => Err(()), // timed out or sink error → connection is dead
    }
}

#[cfg(test)]
mod tests {
    use super::{Cause, SentBlobs, notice};

    #[test]
    fn notice_is_control_free_for_every_cause() {
        // The security invariant: no down-notice can carry a terminal control
        // sequence, whatever a hostile hub/MITM does — because it is built from our
        // own literals plus a u16, never the peer's error text. u16::MAX exercises
        // the only interpolated value.
        for cause in [
            Cause::Connect,
            Cause::Timeout,
            Cause::BadHandshake,
            Cause::Rejected(u16::MAX),
            Cause::Other,
        ] {
            let m = notice(cause);
            assert!(
                !m.chars().any(char::is_control),
                "notice must be control-free: {m:?}"
            );
        }
    }

    #[test]
    fn sent_blobs_dedups_and_fifo_caps() {
        let mut s = SentBlobs::default();
        s.insert("a".into());
        s.insert("a".into()); // dedup: no double-track
        assert!(s.contains("a"));
        // Overflow the cap: the oldest keys evict, newest survive.
        for i in 0..SentBlobs::CAP + 10 {
            s.insert(format!("k{i}"));
        }
        assert!(s.order.len() <= SentBlobs::CAP);
        assert!(!s.contains("a"), "oldest evicted past the cap");
        assert!(
            s.contains(&format!("k{}", SentBlobs::CAP + 9)),
            "newest kept"
        );
    }
}

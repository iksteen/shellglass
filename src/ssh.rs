//! Read-only SSH viewer: `ssh <session-id>@host -p 2222` renders a mirrored
//! session as a live ANSI terminal, no browser needed.
//!
//! The session id in the SSH **username** is the authorization — it's already the
//! public read capability (same one that goes in `/s/<id>`), so there's nothing to
//! prompt for. All auth methods accept; an unknown id is reported in-band after the
//! shell opens rather than as a cryptic "Permission denied". Input is dropped except
//! `q` / Ctrl-C / Ctrl-D, which disconnect.
//!
//! The renderer ([`crate::ansi`]) is pure and tested separately; this module is the
//! russh plumbing: a per-connection handler that tracks the client's terminal size,
//! and a spawned [`view_loop`] that subscribes to the session's [`diff::Live`] as a
//! bare "state changed" tick and repaints the latest snapshot each time. Bursts,
//! `Lagged`, and a slow SSH window all collapse to "render the newest frame" — the
//! source is already 30fps-capped, so no queue or rate limiter is needed.

use crate::model::Frame;
use crate::{ansi, diff, hub};
use anyhow::{Context, Result};
use russh::keys::ssh_key::LineEnding;
use russh::keys::ssh_key::private::Ed25519Keypair;
use russh::keys::{HashAlg, PrivateKey};
use russh::server::{Auth, ChannelOpenHandle, Config, Handler, Msg, Session, run_stream};
use russh::{Channel, ChannelId};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, broadcast, watch};

/// Which sessions the SSH server exposes: the one standalone session (username
/// ignored), or a hub's session table keyed by the id in the username.
#[derive(Clone)]
pub enum Target {
    Single(Arc<diff::Live>),
    Hub(hub::HubState),
}

impl Target {
    fn resolve(&self, user: &str) -> Option<Arc<diff::Live>> {
        match self {
            Target::Single(live) => Some(Arc::clone(live)),
            Target::Hub(hub) => hub.live(user),
        }
    }
}

/// Client terminal size, shared from the handler to the render task (updated by
/// pty-req / window-change). Quit is detected in the render task itself, from the
/// input it drains off the channel — see [`view_loop`].
#[derive(Clone, Copy)]
struct Ctl {
    cols: u16,
    rows: u16,
}

impl Default for Ctl {
    fn default() -> Self {
        Ctl { cols: 80, rows: 24 }
    }
}

/// Resolve the host key and print the connection hint + fingerprint. Called before
/// the PTY switches the terminal to raw mode (so the lines land cleanly), given the
/// bound SSH address and the username to show in the hint (`x` standalone, the
/// literal `<session-id>` for a hub).
pub fn setup(addr: SocketAddr, key_path: Option<&Path>, hint_user: &str) -> Result<PrivateKey> {
    let key = host_key(key_path)?;
    println!(
        "shellglass: read-only SSH view — ssh {hint_user}@{} -p {}",
        addr.ip(),
        addr.port()
    );
    eprintln!(
        "shellglass ssh: host key fingerprint {}",
        key.fingerprint(HashAlg::Sha256)
    );
    Ok(key)
}

/// Cap on concurrent *pre-auth* SSH connections — the SSH analogue of sshd's
/// `MaxStartups`. Auth accepts everyone, so the cheap flood is opening sockets and
/// stalling in the handshake: each holds a socket + task at no cost to the attacker.
/// We hand-roll the accept loop russh's `run_on_socket` would otherwise own so every
/// connection takes a permit for the handshake phase and *releases it the moment auth
/// completes* — established viewers don't count against the cap, only connections
/// still handshaking. At the cap a new connection is dropped immediately (freeing the
/// fd) rather than queued; the inactivity timeout cycles slots held by pre-auth
/// stalls. ponytail: flat cap — plenty for this tool's scale.
const MAX_PREAUTH_CONNS: usize = 1024;

/// Run the SSH viewer server on an already-bound listener until it stops.
pub async fn serve(listener: TcpListener, key: PrivateKey, target: Target) -> Result<()> {
    let config = Arc::new(Config {
        keys: vec![key],
        // Bound every connection's idle time. Its most important job is the pre-auth
        // handshake: russh wraps the initial SSH-banner read in this timeout, so with
        // `None` a peer that opens TCP and never sends its banner holds a task + its
        // buffers forever — an unauthenticated connection-hold flood. A live viewer
        // that only watches never trips it: the 30s keepalive round-trip below counts
        // as received data and resets the timer, so long-lived idle views stay up as
        // long as this stays comfortably above keepalive_interval.
        inactivity_timeout: Some(std::time::Duration::from_secs(90)),
        // Probe idle viewers so a peer that vanished without a FIN (laptop sleep, NAT
        // reboot) is reaped — otherwise `view_loop` blocks in ticks.recv() with
        // nothing to write and never notices the dead socket. Doubles as the liveness
        // signal that keeps a silently-watching viewer under inactivity_timeout.
        keepalive_interval: Some(std::time::Duration::from_secs(30)),
        nodelay: true,
        ..Default::default()
    });
    let slots = Arc::new(Semaphore::new(MAX_PREAUTH_CONNS));
    loop {
        let (socket, _peer) = match listener.accept().await {
            Ok(pair) => pair,
            // A transient accept error (momentary fd exhaustion, etc.) shouldn't kill
            // the whole SSH server — log and keep accepting.
            Err(e) => {
                eprintln!("shellglass ssh: accept error: {e}");
                continue;
            }
        };
        // At the cap: drop the socket now (its fd closes) rather than hold it — not
        // accumulating fds is the whole point. ponytail: silent drop; logging every
        // rejected connection under a flood would be its own log-spam DoS.
        let Ok(permit) = Arc::clone(&slots).try_acquire_owned() else {
            drop(socket);
            continue;
        };
        if config.nodelay {
            let _ = socket.set_nodelay(true);
        }
        // The permit moves into the handler, which drops it on auth success (or when
        // the handler drops — a handshake that fails or stalls out — releasing the
        // pre-auth slot either way).
        let handler = SshHandler::new(target.clone(), permit);
        let config = Arc::clone(&config);
        tokio::spawn(async move {
            // Handshake failure (bad SSH banner, kex) drops the connection silently —
            // every port scanner trips it, so logging would be noise.
            if let Ok(session) = run_stream(config, socket, handler).await {
                let _ = session.await;
            }
        });
    }
}

struct SshHandler {
    target: Target,
    user: String,
    channel: Option<Channel<Msg>>,
    saw_pty: bool,
    ctl: watch::Sender<Ctl>,
    /// Held for the pre-auth handshake; dropped on auth success (see [`accept`]) so
    /// established connections don't count against [`MAX_PREAUTH_CONNS`].
    ///
    /// [`accept`]: SshHandler::accept
    permit: Option<OwnedSemaphorePermit>,
}

impl SshHandler {
    fn new(target: Target, permit: OwnedSemaphorePermit) -> Self {
        let (ctl, _) = watch::channel(Ctl::default());
        SshHandler {
            target,
            user: String::new(),
            channel: None,
            saw_pty: false,
            ctl,
            permit: Some(permit),
        }
    }

    /// Accept auth: record the username (the id is the capability) and release the
    /// pre-auth permit — the handshake is done, so this connection stops occupying a
    /// startup slot.
    fn accept(&mut self, user: &str) -> Auth {
        self.user = user.to_string();
        self.permit = None;
        Auth::Accept
    }
}

impl SshHandler {
    /// Store a client-reported terminal size (pty-req / window-change). Zero
    /// dimensions (some clients send them) keep the current value.
    fn set_size(&self, cols: u32, rows: u32) {
        let cols = u16::try_from(cols).unwrap_or(u16::MAX);
        let rows = u16::try_from(rows).unwrap_or(u16::MAX);
        self.ctl.send_modify(|c| {
            if cols > 0 {
                c.cols = cols;
            }
            if rows > 0 {
                c.rows = rows;
            }
        });
    }
}

impl Handler for SshHandler {
    type Error = anyhow::Error;

    // The id in the username is the capability, so every method accepts; `auth_none`
    // means `ssh id@host` connects with no password prompt.
    async fn auth_none(&mut self, user: &str) -> Result<Auth> {
        Ok(self.accept(user))
    }

    async fn auth_password(&mut self, user: &str, _password: &str) -> Result<Auth> {
        Ok(self.accept(user))
    }

    async fn auth_publickey(
        &mut self,
        user: &str,
        _key: &russh::keys::ssh_key::PublicKey,
    ) -> Result<Auth> {
        Ok(self.accept(user))
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<()> {
        self.channel = Some(channel);
        reply.accept().await;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn pty_request(
        &mut self,
        channel: ChannelId,
        _term: &str,
        col_width: u32,
        row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _modes: &[(russh::Pty, u32)],
        session: &mut Session,
    ) -> Result<()> {
        self.saw_pty = true;
        self.set_size(col_width, row_height);
        session.channel_success(channel)?;
        Ok(())
    }

    async fn window_change_request(
        &mut self,
        _channel: ChannelId,
        col_width: u32,
        row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _session: &mut Session,
    ) -> Result<()> {
        // OpenSSH sends window-change without want_reply — no channel_success.
        self.set_size(col_width, row_height);
        Ok(())
    }

    async fn shell_request(&mut self, channel: ChannelId, session: &mut Session) -> Result<()> {
        // Every branch answers the shell-req (want_reply) so strict clients don't
        // hang: channel_success then an in-band message + close, or channel_failure.
        if !self.saw_pty {
            let _ = session.channel_success(channel);
            let _ = session.data(
                channel,
                b"shellglass: a tty is required (don't use ssh -T)\r\n".to_vec(),
            );
            let _ = session.close(channel);
            return Ok(());
        }
        match self.target.resolve(&self.user) {
            None => {
                let _ = session.channel_success(channel);
                let _ = session.data(channel, b"shellglass: unknown session id\r\n".to_vec());
                let _ = session.eof(channel);
                let _ = session.close(channel);
            }
            // ponytail: one session channel per connection (the common `ssh id@host`
            // case). If a multiplexing client opens a second before shell-req, the
            // stored channel is the wrong one — fail the request rather than render
            // to a channel the client didn't attach to.
            Some(live) => match self.channel.take() {
                Some(chan) => {
                    session.channel_success(channel)?;
                    tokio::spawn(view_loop(chan, live, self.ctl.subscribe()));
                }
                None => {
                    let _ = session.channel_failure(channel);
                }
            },
        }
        Ok(())
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        _data: &[u8],
        session: &mut Session,
    ) -> Result<()> {
        let _ = session.channel_success(channel);
        let _ = session.data(
            channel,
            b"shellglass: read-only viewer; connect without a command\r\n".to_vec(),
        );
        let _ = session.close(channel);
        Ok(())
    }

    // No `data` override: input is drained (and quit-detected) by the render task,
    // which reads the channel directly. Relying on `Handler::data` alone would leave
    // the channel's own inbound buffer unread — russh enqueues every byte there too
    // (a `data.clone()` before the callback), and once it fills (default 100) russh's
    // per-connection select blocks in `chan.send().await`, wedging the whole
    // connection bidirectionally. Reading the channel is what actually drains it.
}

/// Render `live` to the SSH channel until the client quits or the connection drops.
/// Subscribes to ticks first, then always paints the latest snapshot — see the module
/// docs for why bursts/lag collapse to "newest frame".
///
/// Crucially it also **reads** the channel every iteration. That drains russh's
/// per-channel inbound buffer (which it fills on every keystroke regardless of the
/// `data` callback); leaving it unread wedges the whole connection once it hits the
/// buffer cap. The read doubles as quit detection.
async fn view_loop(
    mut channel: Channel<Msg>,
    live: Arc<diff::Live>,
    mut ctl: watch::Receiver<Ctl>,
) {
    // make_writer returns a 'static writer (owns a sender clone), so it doesn't borrow
    // `channel` — leaving `channel` free to lend `make_reader` a &mut below. Pin so the
    // AsyncWriteExt methods (which need Unpin) work.
    let mut w = Box::pin(channel.make_writer());
    // Alt screen + hidden cursor + clear.
    if w.write_all(b"\x1b[?1049h\x1b[?25l\x1b[2J").await.is_err() {
        return;
    }
    let mut ticks = live.ticks();
    {
        let mut reader = Box::pin(channel.make_reader());
        let mut inbuf = [0u8; 256];
        let mut last: Option<Arc<Frame>> = None;
        loop {
            let (cols, rows) = {
                let c = *ctl.borrow_and_update();
                (c.cols, c.rows)
            };
            let frame = live.frame();
            let changed = last.as_ref().is_none_or(|l| !Arc::ptr_eq(l, &frame));
            if changed {
                let bytes = ansi::paint(last.as_deref(), &frame, (cols, rows));
                // write_all is the backpressure point: while it blocks on the SSH
                // window, ticks pile up and the next iteration skips to the latest frame.
                if w.write_all(bytes.as_bytes()).await.is_err() || w.flush().await.is_err() {
                    break;
                }
                last = Some(frame);
            }
            tokio::select! {
                r = ticks.recv() => match r {
                    Ok(_) => {}                                        // tick: repaint latest
                    Err(broadcast::error::RecvError::Lagged(_)) => {}  // fell behind: catch up
                    Err(broadcast::error::RecvError::Closed) => break, // publisher gone
                },
                // Resize → full repaint on the next iteration.
                res = ctl.changed() => {
                    if res.is_err() {
                        break;
                    }
                    last = None;
                }
                // Drain client input (keeps russh's inbound buffer from filling). A lone
                // q / Ctrl-C / Ctrl-D quits; anything else — pastes, arrow-key/SS3
                // sequences, mouse reports — is read and discarded (read-only viewer).
                res = reader.read(&mut inbuf) => match res {
                    Ok(0) | Err(_) => break, // EOF / error → client gone
                    Ok(n) if matches!(&inbuf[..n], [b'q'] | [0x03] | [0x04]) => break,
                    Ok(_) => {}
                }
            }
        }
        // `reader` drops here, releasing its &mut borrow of `channel` for the cleanup.
    }
    // Best-effort restore: leave alt screen, show cursor, end the channel.
    let _ = w.write_all(b"\x1b[?1049l\x1b[?25h").await;
    let _ = w.flush().await;
    let _ = channel.eof().await;
    let _ = channel.close().await;
}

// ── host key ─────────────────────────────────────────────────────────────────

/// Resolve the SSH host key: an explicit `--ssh-host-key` path (loaded, or generated
/// then persisted 0600 on first run), else a generated ed25519 key persisted under
/// `$XDG_STATE_HOME/shellglass/` so the fingerprint survives restarts.
///
/// This never borrows the machine's real `/etc/ssh` host key: that key is the host's
/// genuine SSH identity, and binding it to a separate accept-any read-only viewer
/// would silently extend the host's trust to this endpoint. A dedicated key keeps the
/// viewer's identity distinct.
pub fn host_key(path: Option<&Path>) -> Result<PrivateKey> {
    let p = match path {
        Some(p) => p.to_path_buf(),
        None => state_key_path()?,
    };
    if p.exists() {
        return load_key(&p);
    }
    let key = generate_ed25519()?;
    persist(&key, &p)?;
    Ok(key)
}

fn load_key(p: &Path) -> Result<PrivateKey> {
    PrivateKey::read_openssh_file(p).with_context(|| format!("loading SSH host key {p:?}"))
}

/// ponytail: ed25519-only generation — the one algorithm worth generating, seeded
/// from the OS RNG already in use for `gen-key`.
fn generate_ed25519() -> Result<PrivateKey> {
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed)
        .map_err(|e| anyhow::anyhow!("reading OS randomness for the SSH host key: {e}"))?;
    Ok(PrivateKey::from(Ed25519Keypair::from_seed(&seed)))
}

fn persist(key: &PrivateKey, p: &Path) -> Result<()> {
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating host-key directory {dir:?}"))?;
    }
    let pem = key
        .to_openssh(LineEnding::LF)
        .context("encoding host key")?;
    // Create the file 0600 up front — a write-then-chmod leaves a window where the
    // private key is world-readable, and a swallowed chmod error leaves it so.
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(p)
            .with_context(|| format!("writing host key {p:?}"))?;
        f.write_all(pem.as_bytes())
            .with_context(|| format!("writing host key {p:?}"))?;
    }
    #[cfg(not(unix))]
    std::fs::write(p, pem.as_bytes()).with_context(|| format!("writing host key {p:?}"))?;
    Ok(())
}

fn state_key_path() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .context("no XDG_STATE_HOME or HOME to store the SSH host key")?;
    Ok(base.join("shellglass").join("ssh_host_ed25519_key"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_key_round_trips_through_openssh() {
        let key = generate_ed25519().unwrap();
        let dir = std::env::temp_dir().join(format!("sg-ssh-test-{}", std::process::id()));
        let path = dir.join("host_key");
        persist(&key, &path).unwrap();
        let loaded = load_key(&path).unwrap();
        assert_eq!(
            key.fingerprint(HashAlg::Sha256),
            loaded.fingerprint(HashAlg::Sha256),
            "persisted key round-trips to the same fingerprint"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

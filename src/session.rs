//! Detachable session support: the `dtach(1)` model bolted onto the push
//! pipeline. [`start_detached`] runs the mirrored command in a PTY that owns *no*
//! local terminal — it keeps streaming frames to the hub with zero local viewers,
//! and listens on a unix socket. [`attach`] connects a real terminal to that
//! socket, driving the PTY's input and size; on detach (the `Ctrl-\` key, same as
//! dtach/abduco, so it doesn't clash with tmux/screen/zellij prefixes) the owner
//! keeps running and simply holds the last attached dimensions.
//!
//! Only ONE client may be attached at a time. A second `attach` is rejected with
//! a message unless it passes `--force`, which detaches the incumbent first.
//!
//! Because every interactive client goes *through* this socket, the PTY is the one
//! and only client of whatever multiplexer runs inside it — so there is no
//! smallest/largest size arbitration to do: size is "the attached terminal, or the
//! last one that was attached".
//!
//! Unix only (unix-domain sockets + termios). The last successfully attached
//! terminal's graphics capabilities and geometry remain active after detach, and
//! are replaced atomically only when a later attach finishes probing.

use crate::model::Frame;
use crate::pty::{
    Caps, DaRewriter, ImageReady, RawMode, ScreenState, TermGeom, TerminalProfile, image_worker,
    probe_caps, term_geom, transcode_for,
};
use crate::source::{SinkStatus, SourceSession};
use anyhow::{Context, Result};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use tokio::sync::watch;

/// The detach hotkey: `Ctrl-\` (FS, 0x1c) — dtach/abduco's default, chosen so it
/// doesn't collide with tmux (`Ctrl-b`), screen (`Ctrl-a`) or zellij (`Ctrl-p/q`).
const DETACH_KEY: u8 = 0x1c;

// Wire framing (both directions): [tag: u8][len: u32 BE][payload].
// client -> owner:
const C_INPUT: u8 = 0; //   raw input bytes
const C_RESIZE: u8 = 1; //  {cols,rows,pixel-width,pixel-height: u16 BE}
const C_DETACH: u8 = 2; //  user detached (Ctrl-\)
const C_HELLO: u8 = 3; //   first frame: {force:u8, protocol-version:u8}
const C_PROFILE: u8 = 4; // accepted client's terminal profile
// owner -> client:
const S_DATA: u8 = 0; //    screen bytes (write verbatim to stdout)
const S_ACCEPTED: u8 = 1; //attach granted; streaming follows
const S_REJECTED: u8 = 2; //attach refused; payload is the reason
const S_KICKED: u8 = 3; //  you were force-detached by another client

/// The attach socket is private/local, but the binaries are shipped separately:
/// reject skew explicitly instead of failing halfway through the two-phase attach.
const ATTACH_PROTOCOL_VERSION: u8 = 2;
const PROFILE_LEN: usize = 17;

fn write_frame<W: Write>(w: &mut W, tag: u8, payload: &[u8]) -> std::io::Result<()> {
    let len = payload.len() as u32;
    w.write_all(&[tag])?;
    w.write_all(&len.to_be_bytes())?;
    w.write_all(payload)?;
    w.flush()
}

/// Cap on one wire frame's payload. The largest legitimate frame is a
/// full-screen repaint, well under this; anything bigger is a broken or hostile
/// peer and must not translate into an attacker-sized allocation.
const MAX_FRAME_LEN: usize = 1 << 20;

fn read_frame<R: Read>(r: &mut R) -> std::io::Result<(u8, Vec<u8>)> {
    let mut hdr = [0u8; 5];
    r.read_exact(&mut hdr)?;
    let len = u32::from_be_bytes([hdr[1], hdr[2], hdr[3], hdr[4]]) as usize;
    if len > MAX_FRAME_LEN {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "wire frame exceeds MAX_FRAME_LEN",
        ));
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    Ok((hdr[0], payload))
}

/// Fixed profile wire shape: flags; fg-present+RGB; bg-present+RGB; then
/// cols/rows/pixel-width/pixel-height as big-endian u16s.
fn encode_profile(profile: TerminalProfile) -> [u8; PROFILE_LEN] {
    let mut out = [0u8; PROFILE_LEN];
    out[0] = u8::from(profile.caps.kitty)
        | (u8::from(profile.caps.iterm) << 1)
        | (u8::from(profile.caps.sixel) << 2);
    if let Some(rgb) = profile.caps.default_fg {
        out[1] = 1;
        out[2..5].copy_from_slice(&[rgb.0, rgb.1, rgb.2]);
    }
    if let Some(rgb) = profile.caps.default_bg {
        out[5] = 1;
        out[6..9].copy_from_slice(&[rgb.0, rgb.1, rgb.2]);
    }
    for (i, v) in [
        profile.geom.cols,
        profile.geom.rows,
        profile.geom.px_w,
        profile.geom.px_h,
    ]
    .into_iter()
    .enumerate()
    {
        out[9 + i * 2..11 + i * 2].copy_from_slice(&v.to_be_bytes());
    }
    out
}

fn decode_profile(bytes: &[u8]) -> Option<TerminalProfile> {
    if bytes.len() != PROFILE_LEN || bytes[0] & !0b111 != 0 || bytes[1] > 1 || bytes[5] > 1 {
        return None;
    }
    let word = |i: usize| u16::from_be_bytes([bytes[i], bytes[i + 1]]);
    let geom = TermGeom {
        cols: word(9),
        rows: word(11),
        px_w: word(13),
        px_h: word(15),
    };
    if geom.cols == 0 || geom.rows == 0 || geom.px_w == 0 || geom.px_h == 0 {
        return None;
    }
    Some(TerminalProfile {
        caps: Caps {
            kitty: bytes[0] & 1 != 0,
            iterm: bytes[0] & 2 != 0,
            sixel: bytes[0] & 4 != 0,
            default_fg: (bytes[1] == 1).then_some((bytes[2], bytes[3], bytes[4])),
            default_bg: (bytes[5] == 1).then_some((bytes[6], bytes[7], bytes[8])),
        },
        geom,
    })
}

/// A registered attach client's output side (owned by the core thread).
struct ClientSink {
    stream: UnixStream,
}

impl ClientSink {
    fn send_data(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        write_frame(&mut self.stream, S_DATA, bytes)
    }
    fn send_accepted(&mut self) -> std::io::Result<()> {
        write_frame(&mut self.stream, S_ACCEPTED, &[ATTACH_PROTOCOL_VERSION])
    }
    fn send_rejected(&mut self, reason: &str) -> std::io::Result<()> {
        write_frame(&mut self.stream, S_REJECTED, reason.as_bytes())
    }
    fn send_kicked(&mut self) -> std::io::Result<()> {
        write_frame(&mut self.stream, S_KICKED, &[])
    }
}

/// Messages into the single core thread (owns the [`ScreenState`]).
enum Core {
    Data(Vec<u8>),
    ImageReady(ImageReady),
    Input {
        cid: u64,
        bytes: Vec<u8>,
    },
    ClientResize {
        cid: u64,
        geom: TermGeom,
    },
    /// A connection wants to attach. The core is the sole owner of the
    /// one-client-at-a-time policy: it accepts (optionally kicking the incumbent
    /// when `force`) or rejects, replying so the handler knows whether to pump.
    AttachRequest {
        cid: u64,
        force: bool,
        sink: ClientSink,
        reply: mpsc::Sender<bool>,
    },
    /// Phase two of attach. Reserving a slot does not alter cached terminal
    /// state; only a valid profile makes the client live and replaces it.
    Profile {
        cid: u64,
        profile: TerminalProfile,
        reply: mpsc::Sender<bool>,
    },
    Detach {
        cid: u64,
    },
    HubDown(String),
    HubUp,
    Shutdown,
}

/// Messages into the PTY-control thread (sole owner of the master + writer).
#[derive(Debug)]
enum PtyCmd {
    Input(Vec<u8>),
    Resize(TermGeom),
    Profile {
        geom: TermGeom,
        advertise_sixel: bool,
    },
}

struct AttachedClient {
    cid: u64,
    sink: ClientSink,
    live: bool,
}

/// Default runtime directory for per-session files (socket, daemon log).
fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
}

/// Default socket path for a session id, when `--socket` isn't given.
pub fn default_socket_path(id: &str) -> PathBuf {
    runtime_dir().join(format!("shellglass-{id}.sock"))
}

/// Default log path for a `--daemon` session, when `--log-file` isn't given.
pub fn default_log_path(id: &str) -> PathBuf {
    runtime_dir().join(format!("shellglass-{id}.log"))
}

/// Daemonize the current (single-threaded) process: fork, `setsid` (detach the
/// controlling terminal, so an SSH logout's SIGHUP can't reach it), fork again
/// (so the daemon is not a session leader and can never reacquire a tty), `chdir
/// /`, and redirect stdio to `log_path`. The ORIGINAL parent prints `info` and
/// exits(0) so the launching shell returns at once; this function returns ONLY in
/// the fully-detached daemon.
///
/// MUST be called before any threads exist (notably the tokio runtime): after a
/// fork only the calling thread survives, so a threaded runtime forked here would
/// be corrupt.
pub fn daemonize(info: &str, log_path: &Path) -> Result<()> {
    // First fork: the parent reports to the user and leaves; the child continues.
    match unsafe { libc::fork() } {
        -1 => return Err(std::io::Error::last_os_error()).context("fork"),
        0 => {}
        _ => {
            print!("{info}");
            let _ = std::io::stdout().flush();
            std::process::exit(0);
        }
    }
    // New session — no controlling terminal.
    if unsafe { libc::setsid() } == -1 {
        return Err(std::io::Error::last_os_error()).context("setsid");
    }
    // Second fork: the daemon is a session member, not the leader, so it can never
    // acquire a controlling terminal by opening one.
    match unsafe { libc::fork() } {
        -1 => return Err(std::io::Error::last_os_error()).context("fork"),
        0 => {}
        _ => std::process::exit(0),
    }
    unsafe {
        libc::chdir(c"/".as_ptr());
    }
    redirect_stdio(log_path)
}

/// Point stdin at `/dev/null` and stdout/stderr at the daemon log.
fn redirect_stdio(log_path: &Path) -> Result<()> {
    use std::os::unix::io::AsRawFd;
    if let Some(dir) = log_path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let devnull = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")
        .context("opening /dev/null")?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .with_context(|| format!("opening log file {}", log_path.display()))?;
    // SAFETY: dup2 onto the standard fds; the source Files close on drop, leaving
    // 0/1/2 pointing at the new targets.
    unsafe {
        libc::dup2(devnull.as_raw_fd(), libc::STDIN_FILENO);
        libc::dup2(log.as_raw_fd(), libc::STDOUT_FILENO);
        libc::dup2(log.as_raw_fd(), libc::STDERR_FILENO);
    }
    Ok(())
}

/// Start the mirrored command in a terminal-less PTY at `size` and expose it on a
/// unix socket for [`attach`]. Returns a [`SourceSession`] — the same producer
/// contract [`crate::pty::start`] satisfies, so the hub pipeline neither knows
/// nor cares that this one owns no terminal.
pub fn start_detached(
    command: &[String],
    socket: &Path,
    size: (u16, u16),
) -> Result<SourceSession> {
    start_detached_with_compat(command, socket, size, false)
}

/// Internal entry point used by the CLI to preserve `--sixel-compat` in
/// detachable mode without changing the public library API.
pub(crate) fn start_detached_with_compat(
    command: &[String],
    socket: &Path,
    size: (u16, u16),
    sixel_compat: bool,
) -> Result<SourceSession> {
    let (cols, rows) = size;
    let pair = native_pty_system()
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("opening pty")?;

    let mut builder = CommandBuilder::new(&command[0]);
    builder.args(&command[1..]);
    if let Ok(cwd) = std::env::current_dir() {
        builder.cwd(cwd);
    }
    if std::env::var_os("TERM").is_none() {
        builder.env("TERM", "xterm-256color");
    }
    let mut child = pair
        .slave
        .spawn_command(builder)
        .context("spawning command")?;
    drop(pair.slave);

    let master = pair.master;
    let mut reader = master.try_clone_reader().context("cloning pty reader")?;
    let writer = master.take_writer().context("taking pty writer")?;

    let (core_tx, core_rx) = mpsc::channel::<Core>();
    let (pty_tx, pty_rx) = mpsc::channel::<PtyCmd>();
    let (frame_tx, frame_rx) = watch::channel(ScreenState::blank_frame(rows, cols));

    // PTY reader -> core.
    {
        let core_tx = core_tx.clone();
        thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if core_tx.send(Core::Data(buf[..n].to_vec())).is_err() {
                            break;
                        }
                    }
                }
            }
        });
    }

    // PTY control: sole owner of the master (resize) and writer (input).
    {
        thread::spawn(move || {
            let mut writer = writer;
            let master = master;
            let mut rewrite_da = false;
            let mut da = DaRewriter::default();
            while let Ok(cmd) = pty_rx.recv() {
                match cmd {
                    PtyCmd::Input(b) => {
                        let bytes = if rewrite_da {
                            da.advertise_sixel(&b)
                        } else {
                            b
                        };
                        let _ = writer.write_all(&bytes);
                        let _ = writer.flush();
                    }
                    PtyCmd::Resize(geom) => {
                        let _ = master.resize(PtySize {
                            rows: geom.rows,
                            cols: geom.cols,
                            pixel_width: geom.px_w,
                            pixel_height: geom.px_h,
                        });
                    }
                    PtyCmd::Profile {
                        geom,
                        advertise_sixel,
                    } => {
                        rewrite_da = advertise_sixel;
                        da = DaRewriter::default();
                        let _ = master.resize(PtySize {
                            rows: geom.rows,
                            cols: geom.cols,
                            pixel_width: geom.px_w,
                            pixel_height: geom.px_h,
                        });
                    }
                }
            }
        });
    }

    // Child waiter -> shutdown.
    {
        let core_tx = core_tx.clone();
        thread::spawn(move || {
            let _ = child.wait();
            let _ = core_tx.send(Core::Shutdown);
        });
    }

    // Socket listener.
    if let Some(dir) = socket.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // A connectable socket means a live owner — refuse to silently steal its
    // attach path; only a dead leftover is stale and safe to clear.
    if socket.exists() {
        if UnixStream::connect(socket).is_ok() {
            anyhow::bail!(
                "a session is already listening on {} (use --socket for a second session)",
                socket.display()
            );
        }
        let _ = std::fs::remove_file(socket);
    }
    let listener = UnixListener::bind(socket)
        .with_context(|| format!("binding socket {}", socket.display()))?;
    // The socket is the write capability to the session (keystroke injection):
    // owner-only, regardless of umask or a custom --socket location.
    let _ = std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600));
    {
        let core_tx = core_tx.clone();
        thread::spawn(move || {
            let mut next_cid: u64 = 0;
            for conn in listener.incoming() {
                let stream = match conn {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                next_cid += 1;
                let cid = next_cid;
                let core_tx = core_tx.clone();
                thread::spawn(move || handle_client(stream, cid, core_tx));
            }
        });
    }

    // Core thread.
    {
        let socket = socket.to_path_buf();
        let pty_tx = pty_tx.clone();
        let ready_tx = core_tx.clone();
        let image_jobs = image_worker(move |ready| ready_tx.send(Core::ImageReady(ready)).is_ok());
        thread::spawn(move || {
            core_thread(
                core_rx,
                pty_tx,
                frame_tx,
                ScreenState::new(rows, cols, image_jobs),
                socket,
                sixel_compat,
            );
        });
    }

    Ok(SourceSession::new(frame_rx, Arc::new(CoreStatus(core_tx))))
}

/// Hub status for a detached owner. There is no local terminal to pause or
/// repaint (that is what [`crate::pty`]'s notifier does), so the events just go
/// to the core thread, which logs them.
struct CoreStatus(mpsc::Sender<Core>);

impl SinkStatus for CoreStatus {
    fn hub_down(&self, msg: &str) {
        let _ = self.0.send(Core::HubDown(msg.to_string()));
    }

    fn hub_up(&self) {
        let _ = self.0.send(Core::HubUp);
    }
}

/// Owner-side per-connection handler. Attach is deliberately two phase: reserve
/// the one client slot, then accept a successfully probed terminal profile. A
/// disconnect between those phases leaves the previous cached profile untouched.
fn handle_client(stream: UnixStream, cid: u64, core_tx: mpsc::Sender<Core>) {
    let sink_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    // Bound sink writes: the core thread is the hub's frame producer, and a
    // stuck client (SIGSTOP, wedged terminal) filling the socket buffer must
    // cost it at most this before the client is dropped — never a frozen mirror.
    let _ = sink_stream.set_write_timeout(Some(Duration::from_secs(1)));
    let mut read_half = stream;
    let mut sink = ClientSink {
        stream: sink_stream,
    };

    // Reject binary skew before touching the client's tty or reserving the slot.
    let force = match read_frame(&mut read_half) {
        Ok((C_HELLO, p)) if p.len() == 2 && p[0] <= 1 && p[1] == ATTACH_PROTOCOL_VERSION => {
            p[0] != 0
        }
        Ok(_) => {
            let _ = sink.send_rejected("incompatible attach protocol version");
            return;
        }
        Err(_) => return,
    };

    let (reply_tx, reply_rx) = mpsc::channel::<bool>();
    if core_tx
        .send(Core::AttachRequest {
            cid,
            force,
            sink,
            reply: reply_tx,
        })
        .is_err()
    {
        return;
    }
    // Rejected (or core gone): the core already wrote the reason to the client.
    if !matches!(reply_rx.recv(), Ok(true)) {
        return;
    }

    // Phase two must be the fixed-shape profile. Any malformed/aborted attach
    // simply releases the reservation and retains the previous cached profile.
    let profile = match read_frame(&mut read_half) {
        Ok((C_PROFILE, p)) => match decode_profile(&p) {
            Some(profile) => profile,
            None => {
                let _ = core_tx.send(Core::Detach { cid });
                return;
            }
        },
        _ => {
            let _ = core_tx.send(Core::Detach { cid });
            return;
        }
    };
    let (profile_tx, profile_rx) = mpsc::channel();
    if core_tx
        .send(Core::Profile {
            cid,
            profile,
            reply: profile_tx,
        })
        .is_err()
        || !matches!(profile_rx.recv(), Ok(true))
    {
        let _ = core_tx.send(Core::Detach { cid });
        return;
    }

    loop {
        match read_frame(&mut read_half) {
            Ok((C_INPUT, payload)) => {
                let _ = core_tx.send(Core::Input {
                    cid,
                    bytes: payload,
                });
            }
            Ok((C_RESIZE, p)) => {
                if let Some(geom) = decode_geom(&p) {
                    let _ = core_tx.send(Core::ClientResize { cid, geom });
                }
            }
            Ok((C_DETACH, _)) => break,
            Ok(_) => {}
            Err(_) => break,
        }
    }
    let _ = core_tx.send(Core::Detach { cid });
}

/// The single owner of the [`ScreenState`] AND the one-client-at-a-time policy.
/// Tees PTY output to the attached client (if any); the screen state itself owns
/// the frame clock, so pacing and fidelity match the local mirror by construction.
fn core_thread(
    rx: mpsc::Receiver<Core>,
    pty_tx: mpsc::Sender<PtyCmd>,
    frame_tx: watch::Sender<Arc<Frame>>,
    mut screen: ScreenState,
    socket: PathBuf,
    sixel_compat: bool,
) {
    let mut client: Option<AttachedClient> = None;

    loop {
        let msg = match screen.wait() {
            Some(d) => match rx.recv_timeout(d) {
                Ok(m) => Some(m),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            },
            None => match rx.recv() {
                Ok(m) => Some(m),
                Err(_) => break,
            },
        };

        match msg {
            Some(Core::Data(b)) => {
                let routed = screen.feed_output(&b);
                let mut terminal_answered = false;
                if let Some(attached) = client.as_mut()
                    && attached.live
                {
                    if attached.sink.send_data(&routed.terminal).is_ok() {
                        terminal_answered = true;
                    } else {
                        client = None;
                    }
                }
                if !routed.app.is_empty() {
                    let _ = pty_tx.send(PtyCmd::Input(routed.app));
                }
                if !terminal_answered && !routed.queries.is_empty() {
                    let _ = pty_tx.send(PtyCmd::Input(routed.queries));
                }
            }
            Some(Core::ImageReady(ready)) => screen.image_ready(ready),
            Some(Core::Input { cid, bytes }) => {
                if matches!(client.as_ref(), Some(c) if c.cid == cid && c.live) {
                    let _ = pty_tx.send(PtyCmd::Input(bytes));
                }
            }
            Some(Core::ClientResize { cid, geom }) => {
                if matches!(client.as_ref(), Some(c) if c.cid == cid && c.live) {
                    screen.set_geometry(geom);
                    let _ = pty_tx.send(PtyCmd::Resize(geom));
                }
            }
            Some(Core::AttachRequest {
                cid,
                force,
                mut sink,
                reply,
            }) => {
                if client.is_some() && !force {
                    let _ = sink.send_rejected(
                        "a client is already attached; use `shellglass attach --force` to take over",
                    );
                    let _ = reply.send(false); // sink dropped -> closes the refused connection
                } else {
                    if let Some(mut old) = client.take() {
                        let _ = old.sink.send_kicked(); // force takeover: notify incumbent
                    }
                    // Accepted means reserved, not live: no PTY output is sent and
                    // no cached capability/geometry changes until Profile.
                    let accepted = sink.send_accepted().is_ok();
                    if accepted {
                        client = Some(AttachedClient {
                            cid,
                            sink,
                            live: false,
                        });
                    }
                    let _ = reply.send(accepted);
                }
            }
            Some(Core::Profile {
                cid,
                profile,
                reply,
            }) => {
                let pending = matches!(client.as_ref(), Some(c) if c.cid == cid && !c.live);
                let transcode = transcode_for(profile.caps, sixel_compat);
                let configured = pending
                    && pty_tx
                        .send(PtyCmd::Profile {
                            geom: profile.geom,
                            advertise_sixel: transcode.is_some(),
                        })
                        .is_ok();
                let mut activated = false;
                if configured {
                    // A complete validated probe is the commit point. Size the
                    // screen before producing the new terminal's repaint; if the
                    // peer vanished immediately afterward, retain this successfully
                    // probed profile but release the client slot.
                    screen.set_profile(profile, transcode);
                    screen.set_geometry(profile.geom);
                    if let Some(attached) = client.as_mut() {
                        if attached.sink.send_data(&screen.repaint()).is_ok() {
                            attached.live = true;
                            activated = true;
                        } else {
                            client = None;
                        }
                    }
                } else if pending {
                    client = None;
                }
                let _ = reply.send(activated);
            }
            Some(Core::Detach { cid }) => {
                if matches!(client.as_ref(), Some(c) if c.cid == cid) {
                    client = None; // retain last successful profile and geometry
                }
            }
            Some(Core::HubUp) => {
                // The attached client's screen never showed the outage (HubDown
                // only logs) — nothing to restore; just log the recovery.
                eprintln!("shellglass: hub connection restored");
            }
            Some(Core::HubDown(msg)) => {
                // No local terminal to paint onto; log for the backgrounded owner.
                eprintln!("shellglass: {msg}");
            }
            Some(Core::Shutdown) => {
                let _ = std::fs::remove_file(&socket);
                #[cfg(test)]
                break;
                #[cfg(not(test))]
                std::process::exit(0);
            }
            None => {}
        }

        if let Some(frame) = screen.due_frame() {
            let _ = frame_tx.send(frame);
        }
    }
    let _ = std::fs::remove_file(&socket);
}

fn encode_geom(geom: TermGeom) -> [u8; 8] {
    let mut out = [0u8; 8];
    for (i, v) in [geom.cols, geom.rows, geom.px_w, geom.px_h]
        .into_iter()
        .enumerate()
    {
        out[i * 2..i * 2 + 2].copy_from_slice(&v.to_be_bytes());
    }
    out
}

fn decode_geom(bytes: &[u8]) -> Option<TermGeom> {
    if bytes.len() != 8 {
        return None;
    }
    let word = |i: usize| u16::from_be_bytes([bytes[i], bytes[i + 1]]);
    let geom = TermGeom {
        cols: word(0),
        rows: word(2),
        px_w: word(4),
        px_h: word(6),
    };
    (geom.cols > 0 && geom.rows > 0 && geom.px_w > 0 && geom.px_h > 0).then_some(geom)
}

fn send_resize(w: &mut UnixStream, geom: TermGeom) -> std::io::Result<()> {
    write_frame(w, C_RESIZE, &encode_geom(geom))
}

/// Attach the current terminal to a detached session's socket. Returns when the
/// user detaches (`Ctrl-\`); exits the process if the session ends or the attach
/// is refused (another client is attached and `force` is false).
pub fn attach(socket: &Path, force: bool) -> Result<()> {
    let stream = UnixStream::connect(socket)
        .with_context(|| format!("connecting to session socket {}", socket.display()))?;
    let mut writer = stream.try_clone().context("cloning socket")?;

    // Handshake BEFORE touching the terminal, so a rejection prints cleanly.
    write_frame(
        &mut writer,
        C_HELLO,
        &[force as u8, ATTACH_PROTOCOL_VERSION],
    )
    .context("sending hello")?;

    let mut reader = stream;
    match read_frame(&mut reader) {
        Ok((S_ACCEPTED, p)) if p == [ATTACH_PROTOCOL_VERSION] => {}
        Ok((S_REJECTED, msg)) => {
            eprintln!("shellglass: {}", String::from_utf8_lossy(&msg));
            std::process::exit(1);
        }
        _ => anyhow::bail!("unexpected response from session owner"),
    }

    let raw = Arc::new(RawMode::acquire());
    let profile = TerminalProfile {
        caps: probe_caps(),
        geom: term_geom().unwrap_or(TermGeom {
            cols: 80,
            rows: 24,
            px_w: 640,
            px_h: 384,
        }),
    };
    if let Err(error) = write_frame(&mut writer, C_PROFILE, &encode_profile(profile)) {
        raw.leave();
        return Err(error).context("sending terminal profile");
    }
    // Set the instant we initiate a `Ctrl-\` detach, BEFORE the owner can react
    // and close the socket — so the reader thread's EOF is recognized as our own
    // detach (main prints the notice) and not misreported as the session ending.
    let detaching = Arc::new(AtomicBool::new(false));

    // Socket -> stdout. Handles a force-eviction and a genuine session end
    // distinctly, and stays silent on our own detach (main prints that).
    {
        let raw = raw.clone();
        let detaching = detaching.clone();
        thread::spawn(move || {
            let mut out = std::io::stdout();
            loop {
                match read_frame(&mut reader) {
                    Ok((S_DATA, payload)) => {
                        let _ = out.write_all(&payload);
                        let _ = out.flush();
                    }
                    Ok((S_KICKED, _)) => {
                        raw.leave();
                        let _ = out.write_all(
                            b"\r\n[shellglass: detached \xe2\x80\x94 another client took over with --force]\r\n",
                        );
                        let _ = out.flush();
                        std::process::exit(0);
                    }
                    Ok(_) => {}
                    Err(_) => {
                        if detaching.load(Ordering::SeqCst) {
                            return; // our own detach; main prints "[detached]"
                        }
                        raw.leave();
                        let _ = out.write_all(b"\r\n[shellglass: session ended]\r\n");
                        let _ = out.flush();
                        std::process::exit(0);
                    }
                }
            }
        });
    }

    // SIGWINCH -> forward resizes.
    {
        let mut writer = writer.try_clone().context("cloning socket")?;
        thread::spawn(move || {
            let Ok(mut signals) =
                signal_hook::iterator::Signals::new([signal_hook::consts::SIGWINCH])
            else {
                return;
            };
            let mut last = profile.geom;
            for _ in &mut signals {
                if let Some(geom) = term_geom()
                    && geom != last
                {
                    last = geom;
                    let _ = send_resize(&mut writer, geom);
                }
            }
        });
    }

    // stdin -> socket, intercepting the detach key.
    let mut stdin = std::io::stdin();
    let mut buf = [0u8; 4096];
    loop {
        match stdin.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if let Some(pos) = buf[..n].iter().position(|&b| b == DETACH_KEY) {
                    detaching.store(true, Ordering::SeqCst);
                    if pos > 0 {
                        let _ = write_frame(&mut writer, C_INPUT, &buf[..pos]);
                    }
                    let _ = write_frame(&mut writer, C_DETACH, &[]);
                    break;
                }
                if write_frame(&mut writer, C_INPUT, &buf[..n]).is_err() {
                    break;
                }
            }
        }
    }

    raw.leave(); // restore the tty (RawMode has no Drop)
    println!("\r\n[shellglass: detached]\r");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_frame_round_trip() {
        let mut buf = Vec::new();
        write_frame(&mut buf, C_INPUT, b"hello").unwrap();
        let (tag, payload) = read_frame(&mut std::io::Cursor::new(buf)).unwrap();
        assert_eq!(tag, C_INPUT);
        assert_eq!(payload, b"hello");
    }

    #[test]
    fn wire_frame_rejects_oversized_length() {
        let hdr = [S_DATA, 0xff, 0xff, 0xff, 0xff];
        let err = read_frame(&mut std::io::Cursor::new(hdr.to_vec())).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    fn profile(kitty: bool, sixel: bool, cols: u16) -> TerminalProfile {
        TerminalProfile {
            caps: Caps {
                kitty,
                iterm: false,
                sixel,
                default_fg: Some((1, 2, 3)),
                default_bg: Some((4, 5, 6)),
            },
            geom: TermGeom {
                cols,
                rows: 30,
                px_w: cols * 8,
                px_h: 480,
            },
        }
    }

    #[test]
    fn profile_wire_shape_round_trips_and_rejects_invalid_geometry() {
        let expected = profile(true, true, 100);
        assert_eq!(decode_profile(&encode_profile(expected)), Some(expected));
        let mut invalid = encode_profile(expected);
        invalid[9..11].copy_from_slice(&0u16.to_be_bytes());
        assert_eq!(decode_profile(&invalid), None);
        assert_eq!(
            decode_geom(&encode_geom(expected.geom)),
            Some(expected.geom)
        );
    }

    /// Send one AttachRequest to a running core, returning our end of the
    /// client socket and the core's accept/reject verdict.
    fn attach_request(core_tx: &mpsc::Sender<Core>, cid: u64, force: bool) -> (UnixStream, bool) {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let (reply_tx, reply_rx) = mpsc::channel();
        core_tx
            .send(Core::AttachRequest {
                cid,
                force,
                sink: ClientSink { stream: theirs },
                reply: reply_tx,
            })
            .unwrap();
        (ours, reply_rx.recv().unwrap())
    }

    fn install_profile(core_tx: &mpsc::Sender<Core>, cid: u64, profile: TerminalProfile) -> bool {
        let (reply, result) = mpsc::channel();
        core_tx
            .send(Core::Profile {
                cid,
                profile,
                reply,
            })
            .unwrap();
        result.recv().unwrap()
    }

    fn test_core() -> (
        mpsc::Sender<Core>,
        mpsc::Receiver<PtyCmd>,
        watch::Receiver<Arc<Frame>>,
    ) {
        let (core_tx, core_rx) = mpsc::channel();
        let (pty_tx, pty_rx) = mpsc::channel();
        let (frame_tx, frame_rx) = watch::channel(ScreenState::blank_frame(24, 80));
        let (jobs, _job_rx) = mpsc::channel();
        let sock = std::env::temp_dir().join(format!(
            "shellglass-test-{}-{:?}.sock",
            std::process::id(),
            thread::current().id()
        ));
        thread::spawn(move || {
            core_thread(
                core_rx,
                pty_tx,
                frame_tx,
                ScreenState::new(24, 80, jobs),
                sock,
                false,
            );
        });
        (core_tx, pty_rx, frame_rx)
    }

    #[test]
    fn attach_is_two_phase_and_force_still_enforces_one_client() {
        let (core_tx, pty_rx, _frames) = test_core();

        // Reservation neither resizes nor activates terminal output.
        let (mut first, ok) = attach_request(&core_tx, 1, false);
        assert!(ok);
        let (tag, version) = read_frame(&mut first).unwrap();
        assert_eq!(tag, S_ACCEPTED);
        assert_eq!(version, [ATTACH_PROTOCOL_VERSION]);
        let unexpected = pty_rx.try_recv();
        assert!(
            unexpected.is_err(),
            "unexpected PTY command: {unexpected:?}"
        );

        // A reserved client owns the slot just like a live one.
        let (mut second, ok) = attach_request(&core_tx, 2, false);
        assert!(!ok);
        let (tag, reason) = read_frame(&mut second).unwrap();
        assert_eq!(tag, S_REJECTED);
        assert!(!reason.is_empty());
        assert!(
            pty_rx.try_recv().is_err(),
            "rejected attach must not resize"
        );

        // Completing the profile is the first point at which geometry/caps apply.
        assert!(install_profile(&core_tx, 1, profile(true, false, 100)));
        assert!(matches!(
            pty_rx.recv().unwrap(),
            PtyCmd::Profile {
                geom: TermGeom { cols: 100, .. },
                advertise_sixel: false
            }
        ));
        let (tag, _) = read_frame(&mut first).unwrap();
        assert_eq!(tag, S_DATA); // cells-only repaint

        // A force takeover kicks the incumbent, but the newcomer is pending and
        // still cannot mutate the PTY until it submits a profile.
        let (mut third, ok) = attach_request(&core_tx, 3, true);
        assert!(ok);
        let (tag, _) = read_frame(&mut third).unwrap();
        assert_eq!(tag, S_ACCEPTED);
        let (tag, _) = read_frame(&mut first).unwrap();
        assert_eq!(tag, S_KICKED);
        assert!(pty_rx.try_recv().is_err());
    }

    #[test]
    fn cached_graphics_profile_survives_detach_and_aborted_attach() {
        let (core_tx, pty_rx, _frames) = test_core();
        let (mut first, ok) = attach_request(&core_tx, 10, false);
        assert!(ok);
        let _ = read_frame(&mut first).unwrap();
        assert!(install_profile(&core_tx, 10, profile(true, false, 100)));
        let _ = pty_rx.recv().unwrap();
        let _ = read_frame(&mut first).unwrap();
        core_tx.send(Core::Detach { cid: 10 }).unwrap();

        let query = b"\x1b_Gi=9,a=q,t=d,s=1,v=1;AAAA\x1b\\";
        let expected = b"\x1b_Gi=9;OK\x1b\\";
        core_tx.send(Core::Data(query.to_vec())).unwrap();
        assert!(matches!(
            pty_rx.recv().unwrap(),
            PtyCmd::Input(ref b) if b == expected
        ));
        core_tx.send(Core::Data(b"\x1b[".to_vec())).unwrap();
        assert!(pty_rx.try_recv().is_err());
        core_tx.send(Core::Data(b"c".to_vec())).unwrap();
        assert!(matches!(
            pty_rx.recv().unwrap(),
            PtyCmd::Input(ref b) if b == b"\x1b[?1;2c"
        ));

        // Reserving and then abandoning a terminal with different capabilities
        // must not replace the last successful terminal's cached answer.
        let (mut pending, ok) = attach_request(&core_tx, 11, false);
        assert!(ok);
        let _ = read_frame(&mut pending).unwrap();
        core_tx.send(Core::Data(query.to_vec())).unwrap();
        assert!(matches!(
            pty_rx.recv().unwrap(),
            PtyCmd::Input(ref b) if b == expected
        ));
        core_tx.send(Core::Detach { cid: 11 }).unwrap();

        // A later successful no-kitty profile really does replace it.
        let (mut replacement, ok) = attach_request(&core_tx, 12, false);
        assert!(ok);
        let _ = read_frame(&mut replacement).unwrap();
        assert!(install_profile(&core_tx, 12, profile(false, true, 120)));
        let _ = pty_rx.recv().unwrap();
        let _ = read_frame(&mut replacement).unwrap();
        core_tx.send(Core::Detach { cid: 12 }).unwrap();
        core_tx.send(Core::Data(query.to_vec())).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let unexpected = pty_rx.try_recv();
        assert!(
            unexpected.is_err(),
            "unexpected PTY command: {unexpected:?}"
        );
        core_tx.send(Core::Data(b"\x1b[0c".to_vec())).unwrap();
        assert!(matches!(
            pty_rx.recv().unwrap(),
            PtyCmd::Input(ref b) if b == b"\x1b[?1;2;4c"
        ));
    }

    fn wait_for_frame(
        source: &SourceSession,
        predicate: impl Fn(&crate::model::Grid) -> bool,
    ) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            let current = source.frames.borrow();
            let Frame::Screen(grid) = &**current;
            if predicate(grid) {
                return true;
            }
            drop(current);
            thread::sleep(Duration::from_millis(10));
        }
        false
    }

    /// Empirical regression: drive the actual detachable owner, Unix socket and
    /// child PTY. The child emits one image before any attach (must be ignored),
    /// then after detach asks kitty whether graphics work. It emits the image a
    /// second time only if the owner answers from the retained terminal profile.
    #[test]
    fn real_pty_images_activate_on_attach_and_keep_working_after_detach() {
        const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAACklEQVR4nGMAAQAABQABDQottAAAAABJRU5ErkJggg==";
        let image = format!("\\033_Ga=T,f=100,t=d,c=1,r=1;{PNG}\\033\\\\");
        let script = format!(
            "printf '{image}'; printf 'READY\\r\\n'; IFS= read -r line; \
             sleep 0.1; stty raw -echo; \
             printf '\\033_Gi=9,a=q,t=d,s=1,v=1;AAAA\\033\\\\'; \
             reply=$(dd bs=1 count=11 2>/dev/null); \
             case \"$reply\" in *OK*) printf '{image}';; esac; sleep 0.1"
        );
        let command = vec!["/bin/sh".to_string(), "-c".to_string(), script];
        let socket = std::env::temp_dir().join(format!(
            "shellglass-image-e2e-{}-{:?}.sock",
            std::process::id(),
            thread::current().id()
        ));
        let source = start_detached(&command, &socket, (80, 24)).unwrap();

        assert!(wait_for_frame(&source, |grid| {
            grid.rows
                .iter()
                .flatten()
                .map(|cell| cell.text.as_str())
                .collect::<String>()
                .contains("READY")
        }));
        {
            let current = source.frames.borrow();
            let Frame::Screen(grid) = &**current;
            assert!(
                grid.images.is_empty() && grid.image_data.is_empty(),
                "graphics must remain disabled before the first successful attach"
            );
        }

        let mut client = UnixStream::connect(&socket).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        write_frame(&mut client, C_HELLO, &[0, ATTACH_PROTOCOL_VERSION]).unwrap();
        let (tag, version) = read_frame(&mut client).unwrap();
        assert_eq!((tag, version), (S_ACCEPTED, vec![ATTACH_PROTOCOL_VERSION]));
        write_frame(
            &mut client,
            C_PROFILE,
            &encode_profile(profile(true, false, 80)),
        )
        .unwrap();
        let (tag, _) = read_frame(&mut client).unwrap();
        assert_eq!(tag, S_DATA);

        // Input wakes the child; Detach is ordered behind it in the same owner
        // queue. The child waits briefly, then its query can only be answered by
        // the now-detached owner's cached kitty capability.
        write_frame(&mut client, C_INPUT, b"go\n").unwrap();
        write_frame(&mut client, C_DETACH, &[]).unwrap();
        drop(client);

        assert!(
            wait_for_frame(&source, |grid| {
                !grid.images.is_empty() && !grid.image_data.is_empty()
            }),
            "child received no cached kitty reply, or its image was not mirrored"
        );
    }
}

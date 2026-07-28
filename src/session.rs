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
//! Unix only (unix-domain sockets + termios). Frame fidelity matches the local
//! mirror for text/colors/cursor; inline-image decoding is not wired into the
//! headless owner (see [`crate::pty::HeadlessScreen`]).

use crate::model::Frame;
use crate::pty::{self, HeadlessScreen, RawMode};
use crate::source::{SinkStatus, SourceSession};
use anyhow::{Context, Result};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Instant;
use tokio::sync::watch;

/// The detach hotkey: `Ctrl-\` (FS, 0x1c) — dtach/abduco's default, chosen so it
/// doesn't collide with tmux (`Ctrl-b`), screen (`Ctrl-a`) or zellij (`Ctrl-p/q`).
const DETACH_KEY: u8 = 0x1c;

// Wire framing (both directions): [tag: u8][len: u32 BE][payload].
// client -> owner:
const C_INPUT: u8 = 0; //   raw input bytes
const C_RESIZE: u8 = 1; //  {cols:u16 BE, rows:u16 BE}
const C_DETACH: u8 = 2; //  user detached (Ctrl-\)
const C_HELLO: u8 = 3; //   first frame: {force:u8}
// owner -> client:
const S_DATA: u8 = 0; //    screen bytes (write verbatim to stdout)
const S_ACCEPTED: u8 = 1; //attach granted; streaming follows
const S_REJECTED: u8 = 2; //attach refused; payload is the reason
const S_KICKED: u8 = 3; //  you were force-detached by another client

fn write_frame<W: Write>(w: &mut W, tag: u8, payload: &[u8]) -> std::io::Result<()> {
    let len = payload.len() as u32;
    w.write_all(&[tag])?;
    w.write_all(&len.to_be_bytes())?;
    w.write_all(payload)?;
    w.flush()
}

fn read_frame<R: Read>(r: &mut R) -> std::io::Result<(u8, Vec<u8>)> {
    let mut hdr = [0u8; 5];
    r.read_exact(&mut hdr)?;
    let len = u32::from_be_bytes([hdr[1], hdr[2], hdr[3], hdr[4]]) as usize;
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    Ok((hdr[0], payload))
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
        write_frame(&mut self.stream, S_ACCEPTED, &[])
    }
    fn send_rejected(&mut self, reason: &str) -> std::io::Result<()> {
        write_frame(&mut self.stream, S_REJECTED, reason.as_bytes())
    }
    fn send_kicked(&mut self) -> std::io::Result<()> {
        write_frame(&mut self.stream, S_KICKED, &[])
    }
}

/// Messages into the single core thread (owns the [`HeadlessScreen`]).
enum Core {
    Data(Vec<u8>),
    Resize(u16, u16), // rows, cols (parser side)
    /// A connection wants to attach. The core is the sole owner of the
    /// one-client-at-a-time policy: it accepts (optionally kicking the incumbent
    /// when `force`) or rejects, replying so the handler knows whether to pump.
    AttachRequest {
        cid: u64,
        force: bool,
        cols: u16,
        rows: u16,
        sink: ClientSink,
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
enum PtyCmd {
    Input(Vec<u8>),
    Resize(u16, u16), // rows, cols
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
    let mut child = pair.slave.spawn_command(builder).context("spawning command")?;
    drop(pair.slave);

    let master = pair.master;
    let mut reader = master.try_clone_reader().context("cloning pty reader")?;
    let writer = master.take_writer().context("taking pty writer")?;

    let (core_tx, core_rx) = mpsc::channel::<Core>();
    let (pty_tx, pty_rx) = mpsc::channel::<PtyCmd>();
    let (frame_tx, frame_rx) = watch::channel(HeadlessScreen::blank_frame(rows, cols));

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
        let core_tx = core_tx.clone();
        thread::spawn(move || {
            let mut writer = writer;
            let master = master;
            while let Ok(cmd) = pty_rx.recv() {
                match cmd {
                    PtyCmd::Input(b) => {
                        let _ = writer.write_all(&b);
                        let _ = writer.flush();
                    }
                    PtyCmd::Resize(r, c) => {
                        let _ = master.resize(PtySize {
                            rows: r,
                            cols: c,
                            pixel_width: 0,
                            pixel_height: 0,
                        });
                        let _ = core_tx.send(Core::Resize(r, c));
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
    let _ = std::fs::remove_file(socket); // clear a stale socket
    let listener = UnixListener::bind(socket)
        .with_context(|| format!("binding socket {}", socket.display()))?;
    {
        let core_tx = core_tx.clone();
        let pty_tx = pty_tx.clone();
        thread::spawn(move || {
            let next_cid = AtomicU64::new(0);
            for conn in listener.incoming() {
                let stream = match conn {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                let cid = next_cid.fetch_add(1, Ordering::SeqCst) + 1;
                let core_tx = core_tx.clone();
                let pty_tx = pty_tx.clone();
                thread::spawn(move || handle_client(stream, cid, core_tx, pty_tx));
            }
        });
    }

    // Core thread.
    {
        let socket = socket.to_path_buf();
        thread::spawn(move || core_thread(core_rx, frame_tx, HeadlessScreen::new(rows, cols), socket));
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

/// Owner-side per-connection handler: read the client's hello + initial size, ask
/// the core to attach it, and (if accepted) pump its input/resize/detach frames
/// until it disconnects.
fn handle_client(stream: UnixStream, cid: u64, core_tx: mpsc::Sender<Core>, pty_tx: mpsc::Sender<PtyCmd>) {
    let sink_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    let mut read_half = stream;

    // First frame: hello (carries the --force flag). Tolerate its absence.
    let force = match read_frame(&mut read_half) {
        Ok((C_HELLO, p)) => p.first().copied().unwrap_or(0) != 0,
        Ok(_) => false,
        Err(_) => return,
    };
    // Second frame: the initial terminal size.
    let (mut cols, mut rows) = (80u16, 24u16);
    match read_frame(&mut read_half) {
        Ok((C_RESIZE, p)) if p.len() == 4 => {
            cols = u16::from_be_bytes([p[0], p[1]]);
            rows = u16::from_be_bytes([p[2], p[3]]);
        }
        Ok(_) => {}
        Err(_) => return,
    }
    let _ = pty_tx.send(PtyCmd::Resize(rows, cols));

    let (reply_tx, reply_rx) = mpsc::channel::<bool>();
    if core_tx
        .send(Core::AttachRequest {
            cid,
            force,
            cols,
            rows,
            sink: ClientSink { stream: sink_stream },
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

    loop {
        match read_frame(&mut read_half) {
            Ok((C_INPUT, payload)) => {
                let _ = pty_tx.send(PtyCmd::Input(payload));
            }
            Ok((C_RESIZE, p)) if p.len() == 4 => {
                let c = u16::from_be_bytes([p[0], p[1]]);
                let r = u16::from_be_bytes([p[2], p[3]]);
                let _ = pty_tx.send(PtyCmd::Resize(r, c));
            }
            Ok((C_DETACH, _)) => break,
            Ok(_) => {}
            Err(_) => break,
        }
    }
    let _ = core_tx.send(Core::Detach { cid });
}

/// The single owner of the [`HeadlessScreen`] AND the one-client-at-a-time policy.
/// Tees PTY output to the attached client (if any) and renders frames for the hub
/// at ≤30fps.
fn core_thread(
    rx: mpsc::Receiver<Core>,
    frame_tx: watch::Sender<Arc<Frame>>,
    mut screen: HeadlessScreen,
    socket: PathBuf,
) {
    let mut client: Option<(u64, ClientSink)> = None;
    let mut last_frame = Instant::now();
    let mut dirty = false;

    loop {
        let msg = if dirty {
            match rx.recv_timeout(pty::MIN_FRAME) {
                Ok(m) => Some(m),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        } else {
            match rx.recv() {
                Ok(m) => Some(m),
                Err(_) => break,
            }
        };

        match msg {
            Some(Core::Data(b)) => {
                screen.process(&b);
                if let Some((_, sink)) = client.as_mut()
                    && sink.send_data(&b).is_err()
                {
                    client = None;
                }
                dirty = true;
            }
            Some(Core::Resize(rows, cols)) => {
                screen.set_size(rows, cols);
                dirty = true;
            }
            Some(Core::AttachRequest {
                cid,
                force,
                cols,
                rows,
                mut sink,
                reply,
            }) => {
                if client.is_some() && !force {
                    let _ = sink.send_rejected(
                        "a client is already attached; use `shellglass attach --force` to take over",
                    );
                    let _ = reply.send(false); // sink dropped -> closes the refused connection
                } else {
                    if let Some((_, mut old)) = client.take() {
                        let _ = old.send_kicked(); // force takeover: notify the incumbent
                    }
                    screen.set_size(rows, cols);
                    let _ = sink.send_accepted();
                    let _ = sink.send_data(&screen.repaint());
                    client = Some((cid, sink));
                    let _ = reply.send(true);
                    dirty = true;
                }
            }
            Some(Core::Detach { cid }) => {
                if matches!(client.as_ref(), Some((c, _)) if *c == cid) {
                    client = None; // keep the last attached size; don't resize back
                }
            }
            Some(Core::HubUp) => {
                if let Some((_, sink)) = client.as_mut()
                    && sink.send_data(&screen.repaint()).is_err()
                {
                    client = None;
                }
            }
            Some(Core::HubDown(msg)) => {
                // No local terminal to paint onto; log for the backgrounded owner.
                eprintln!("shellglass: {msg}");
            }
            Some(Core::Shutdown) => {
                let _ = std::fs::remove_file(&socket);
                std::process::exit(0);
            }
            None => {}
        }

        if dirty && last_frame.elapsed() >= pty::MIN_FRAME {
            let _ = frame_tx.send(screen.frame());
            dirty = false;
            last_frame = Instant::now();
        }
    }
    let _ = std::fs::remove_file(&socket);
}

fn send_resize(w: &mut UnixStream, cols: u16, rows: u16) -> std::io::Result<()> {
    let mut p = [0u8; 4];
    p[..2].copy_from_slice(&cols.to_be_bytes());
    p[2..].copy_from_slice(&rows.to_be_bytes());
    write_frame(w, C_RESIZE, &p)
}

/// The controlling terminal's size as (cols, rows), if stdin is a tty.
fn term_size() -> Option<(u16, u16)> {
    // SAFETY: ioctl into a zeroed winsize; only read on success.
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(libc::STDIN_FILENO, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 {
            Some((ws.ws_col, ws.ws_row))
        } else {
            None
        }
    }
}

/// Attach the current terminal to a detached session's socket. Returns when the
/// user detaches (`Ctrl-\`); exits the process if the session ends or the attach
/// is refused (another client is attached and `force` is false).
pub fn attach(socket: &Path, force: bool) -> Result<()> {
    let stream = UnixStream::connect(socket)
        .with_context(|| format!("connecting to session socket {}", socket.display()))?;
    let mut writer = stream.try_clone().context("cloning socket")?;

    // Handshake BEFORE touching the terminal, so a rejection prints cleanly.
    write_frame(&mut writer, C_HELLO, &[force as u8]).context("sending hello")?;
    let (cols, rows) = term_size().unwrap_or((80, 24));
    send_resize(&mut writer, cols, rows).context("sending initial size")?;

    let mut reader = stream;
    match read_frame(&mut reader) {
        Ok((S_ACCEPTED, _)) => {}
        Ok((S_REJECTED, msg)) => {
            eprintln!("shellglass: {}", String::from_utf8_lossy(&msg));
            std::process::exit(1);
        }
        _ => anyhow::bail!("unexpected response from session owner"),
    }

    let raw = Arc::new(RawMode::acquire());
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
            let mut last = (cols, rows);
            for _ in &mut signals {
                if let Some(sz) = term_size()
                    && sz != last
                {
                    last = sz;
                    let _ = send_resize(&mut writer, sz.0, sz.1);
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

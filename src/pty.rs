//! PTY backend: run an interactive command in a pseudo-terminal that you drive
//! from your own terminal, while mirroring its screen to the browser — the
//! `script(1)` model. One PTY feeds a single [`vt100::Parser`], snapshotted as
//! [`Frame`]s at a 30fps cap for the diff/stream pipeline. Unix only (raw mode +
//! `TIOCGWINSZ`).
//!
//! One `screen` thread owns everything that touches the real terminal — the raw
//! mode, stdout, and the vt100 parser — so hub-connection notices can be shown
//! cleanly: on a hub drop it leaves raw mode, clears the screen and prints the
//! error; on reconnect it re-enters raw mode and repaints the screen from the
//! parser (`contents_formatted`), rather than the client's `eprintln!`s corrupting
//! the live session.

use crate::model::Frame;
use anyhow::{Context, Result};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use tokio::sync::watch;

/// Frame cap: coalesce bursts of PTY output into at most ~30 renders per second.
const MIN_FRAME: Duration = Duration::from_millis(33);

/// Everything the screen thread applies. `Data`/`Resize` come from the PTY and the
/// size poller; `HubDown`/`HubUp` from the push client via [`Notifier`]; `Shutdown`
/// from the child waiter.
enum Msg {
    Data(Vec<u8>),
    Resize(u16, u16), // rows, cols
    HubDown(String),
    HubUp,
    Shutdown,
}

/// Lets the push client report hub connection changes to the terminal owner so it
/// can pause/announce/restore cleanly instead of printing into the raw session.
#[derive(Clone)]
pub struct Notifier(mpsc::Sender<Msg>);

impl Notifier {
    /// Hub became unreachable — pause the mirror, drop to cooked mode, show `msg`.
    pub fn hub_down(&self, msg: &str) {
        let _ = self.0.send(Msg::HubDown(msg.to_string()));
    }
    /// Hub is back — restore raw mode and repaint the screen.
    pub fn hub_up(&self) {
        let _ = self.0.send(Msg::HubUp);
    }
}

/// Start an interactive PTY session running `command`. Returns a receiver of the
/// latest screen [`Frame`] plus a [`Notifier`] for hub status. Puts the terminal in
/// raw mode, bridges stdin/stdout, and exits the process when the command exits.
pub fn start(command: &[String]) -> Result<(watch::Receiver<Arc<Frame>>, Notifier)> {
    let (cols, rows) = term_size().unwrap_or((80, 24));
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
    let mut writer = master.take_writer().context("taking pty writer")?;

    // Raw mode now, before the child draws anything.
    let raw = RawMode::acquire();
    let (msg_tx, msg_rx) = mpsc::channel::<Msg>();
    let (frame_tx, frame_rx) = watch::channel(frame_from(&new_parser(rows, cols)));

    // PTY reader → screen thread.
    {
        let msg_tx = msg_tx.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break, // child closed the PTY
                    Ok(n) => {
                        if msg_tx.send(Msg::Data(buf[..n].to_vec())).is_err() {
                            break;
                        }
                    }
                }
            }
        });
    }

    // Our stdin → PTY.
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        let mut stdin = std::io::stdin();
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if writer.write_all(&buf[..n]).is_err() {
                        break;
                    }
                    let _ = writer.flush();
                }
            }
        }
    });

    // Size watcher: reflect terminal resizes into the PTY + parser — immediately on
    // SIGWINCH, with a 1s poll as a fallback. `master` isn't `Sync`, so it stays in
    // this one thread rather than being shared with a separate signal thread.
    {
        let msg_tx = msg_tx.clone();
        let winch = Sigwinch::install();
        std::thread::spawn(move || {
            let mut last = (cols, rows);
            loop {
                winch.wait(Duration::from_secs(1)); // wakes on SIGWINCH or timeout
                match term_size() {
                    Some(size) if size != last => {
                        last = size;
                        let (c, r) = size;
                        let _ = master.resize(PtySize {
                            rows: r,
                            cols: c,
                            pixel_width: 0,
                            pixel_height: 0,
                        });
                        if msg_tx.send(Msg::Resize(r, c)).is_err() {
                            break;
                        }
                    }
                    _ => {}
                }
            }
        });
    }

    // Screen thread: sole owner of the real terminal (raw mode + stdout) and the
    // parser. Tees shell output immediately, renders to the browser at ≤30fps, and
    // handles hub notices with a clean pause/restore.
    std::thread::spawn(move || {
        screen_thread(msg_rx, frame_tx, raw, new_parser(rows, cols));
    });

    // When the command exits, tell the screen thread to restore the terminal + quit.
    {
        let msg_tx = msg_tx.clone();
        std::thread::spawn(move || {
            let _ = child.wait();
            let _ = msg_tx.send(Msg::Shutdown);
        });
    }

    Ok((frame_rx, Notifier(msg_tx)))
}

fn screen_thread(
    msg_rx: mpsc::Receiver<Msg>,
    frame_tx: watch::Sender<Arc<Frame>>,
    raw: RawMode,
    mut parser: vt100::Parser,
) {
    let mut out = std::io::stdout();
    let mut connected = true; // teeing shell output to the terminal
    let mut last_frame = Instant::now();
    let mut dirty = false;
    loop {
        let msg = if dirty {
            match msg_rx.recv_timeout(MIN_FRAME) {
                Ok(m) => Some(m),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        } else {
            match msg_rx.recv() {
                Ok(m) => Some(m),
                Err(_) => break,
            }
        };
        match msg {
            Some(Msg::Data(b)) => {
                parser.process(&b);
                if connected {
                    let _ = out.write_all(&b); // immediate, not rate-limited
                    let _ = out.flush();
                }
                dirty = true;
            }
            Some(Msg::Resize(rows, cols)) => {
                parser.screen_mut().set_size(rows, cols);
                dirty = true;
            }
            Some(Msg::HubDown(msg)) if connected => {
                connected = false;
                raw.leave(); // back to cooked so the notice reads normally
                // The app may have left the screen mid-redraw or with dangling
                // attributes/cursor state, so reset and clear before the notice —
                // we don't know what state the screen is in.
                let _ = out.write_all(b"\x1b[0m\x1b[?25h\x1b[2J\x1b[H");
                let _ = write!(out, "\x1b[33mshellglass: {msg}\x1b[0m\r\n");
                let _ = out.flush();
            }
            Some(Msg::HubUp) if !connected => {
                connected = true;
                raw.enter();
                // Repaint the (now up-to-date) screen over the notice text.
                let _ = out.write_all(b"\x1b[2J\x1b[H");
                let _ = out.write_all(&parser.screen().contents_formatted());
                let _ = out.flush();
                dirty = true;
            }
            Some(Msg::Shutdown) => {
                raw.leave();
                let _ = out.flush();
                std::process::exit(0);
            }
            Some(_) => {} // redundant HubDown/HubUp — ignore
            None => {}    // frame due
        }
        if dirty && last_frame.elapsed() >= MIN_FRAME {
            let _ = frame_tx.send(frame_from(&parser));
            dirty = false;
            last_frame = Instant::now();
        }
    }
}

fn new_parser(rows: u16, cols: u16) -> vt100::Parser {
    vt100::Parser::new(rows, cols, 0)
}

/// Snapshot the PTY screen as a [`Frame`] for the diff/stream pipeline.
fn frame_from(parser: &vt100::Parser) -> Arc<Frame> {
    Arc::new(Frame::Screen(crate::parse::grid_from_screen(
        parser.screen(),
    )))
}

/// Write end of the SIGWINCH self-pipe, read by the async-signal-safe handler.
static SIGWINCH_WRITE_FD: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_sigwinch(_: libc::c_int) {
    let fd = SIGWINCH_WRITE_FD.load(Ordering::Relaxed);
    if fd >= 0 {
        // `write` is async-signal-safe; one byte, best-effort (ignore EAGAIN/etc.).
        let byte = [0u8; 1];
        unsafe {
            libc::write(fd, byte.as_ptr().cast::<libc::c_void>(), 1);
        }
    }
}

/// A self-pipe woken by SIGWINCH: the handler writes a byte and [`Sigwinch::wait`]
/// blocks on the read end (with a fallback timeout) so terminal resizes are picked
/// up instantly instead of only on the next poll tick.
struct Sigwinch {
    read_fd: i32,
}

impl Sigwinch {
    fn install() -> Sigwinch {
        // SAFETY: create a non-blocking pipe and register a SIGWINCH handler that
        // only writes to its write end. SA_RESTART so the signal doesn't turn the
        // other threads' blocking reads into EINTR errors (which would kill them).
        unsafe {
            let mut fds = [0i32; 2];
            if libc::pipe(fds.as_mut_ptr()) != 0 {
                return Sigwinch { read_fd: -1 }; // fall back to pure polling
            }
            let (read_fd, write_fd) = (fds[0], fds[1]);
            libc::fcntl(read_fd, libc::F_SETFL, libc::O_NONBLOCK);
            libc::fcntl(write_fd, libc::F_SETFL, libc::O_NONBLOCK);
            SIGWINCH_WRITE_FD.store(write_fd, Ordering::Relaxed);

            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = on_sigwinch as extern "C" fn(libc::c_int) as usize;
            sa.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&raw mut sa.sa_mask);
            libc::sigaction(libc::SIGWINCH, &raw const sa, std::ptr::null_mut());
            Sigwinch { read_fd }
        }
    }

    /// Block until SIGWINCH fires or `timeout` elapses, then drain the pipe.
    fn wait(&self, timeout: Duration) {
        if self.read_fd < 0 {
            std::thread::sleep(timeout);
            return;
        }
        // SAFETY: poll then drain our own non-blocking pipe fd.
        unsafe {
            let mut pfd = libc::pollfd {
                fd: self.read_fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let ms = timeout.as_millis().min(i32::MAX as u128) as libc::c_int;
            libc::poll(&raw mut pfd, 1, ms);
            let mut buf = [0u8; 64];
            while libc::read(
                self.read_fd,
                buf.as_mut_ptr().cast::<libc::c_void>(),
                buf.len(),
            ) > 0
            {}
        }
    }
}

/// Our controlling terminal's size as (cols, rows), if stdin is a tty.
fn term_size() -> Option<(u16, u16)> {
    // SAFETY: ioctl into a zeroed winsize; we only read the result on success.
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(libc::STDIN_FILENO, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 {
            Some((ws.ws_col, ws.ws_row))
        } else {
            None
        }
    }
}

/// Owns the terminal's raw-mode state: `acquire` enters raw and remembers the
/// original settings; `leave`/`enter` toggle between them for the hub-outage pause.
struct RawMode {
    orig: Option<libc::termios>,
}

impl RawMode {
    fn acquire() -> RawMode {
        // SAFETY: standard termios raw-mode dance on the real stdin fd.
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &raw mut t) != 0 {
                return RawMode { orig: None }; // not a tty (e.g. piped) — leave as-is
            }
            let orig = t;
            let mut rawt = t;
            libc::cfmakeraw(&raw mut rawt);
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw const rawt);
            RawMode { orig: Some(orig) }
        }
    }

    /// Restore the terminal's original (cooked) settings.
    fn leave(&self) {
        if let Some(orig) = self.orig {
            // SAFETY: restoring the saved termios on the same fd.
            unsafe {
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw const orig);
            }
        }
    }

    /// Re-enter raw mode (from the saved original settings).
    fn enter(&self) {
        if let Some(orig) = self.orig {
            // SAFETY: applying a raw variant of the saved termios on the same fd.
            unsafe {
                let mut rawt = orig;
                libc::cfmakeraw(&raw mut rawt);
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw const rawt);
            }
        }
    }
}

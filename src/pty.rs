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
use std::sync::mpsc;
use std::time::{Duration, Instant};
use tokio::sync::watch;

/// Frame cap: coalesce bursts of PTY output into at most ~30 renders per second.
const MIN_FRAME: Duration = Duration::from_millis(33);

/// Reset every input mode an app could have switched on, for the hub-outage pause:
/// normal keypad (`ESC >`), normal cursor keys, bracketed paste off, and all xterm
/// mouse-reporting modes + encodings off. Turning off a mode that isn't on is a
/// no-op, so this is safe to fire blind — the restore side re-arms from the parser
/// (`input_mode_formatted`), which knows the app's actual current modes.
const INPUT_MODES_OFF: &[u8] =
    b"\x1b>\x1b[?1l\x1b[?2004l\x1b[?1000l\x1b[?1001l\x1b[?1002l\x1b[?1003l\x1b[?1005l\x1b[?1006l";

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
    // Inherit our own working directory (the script(1) model). Without this,
    // portable-pty defaults the child's cwd to $HOME and resolves a cwd-relative
    // program path (`./foo`) against $HOME too — so you'd spawn in ~, not where
    // you launched shellglass.
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
    let mut writer = master.take_writer().context("taking pty writer")?;

    // Raw mode now, before the child draws anything.
    let raw = RawMode::acquire();
    // Clear the local terminal so the mirrored session starts from a blank screen,
    // matching the fresh (blank) parser that viewers see.
    {
        use std::io::Write;
        let mut out = std::io::stdout();
        let _ = out.write_all(b"\x1b[2J\x1b[H");
        let _ = out.flush();
    }
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

    // Size watcher: reflect terminal resizes into the PTY + parser on SIGWINCH.
    // `master` isn't `Sync`, so it stays in this one thread rather than being shared
    // with a separate signal thread. If signal registration fails (rare), resize
    // tracking is skipped — the initial size still applies.
    {
        let msg_tx = msg_tx.clone();
        std::thread::spawn(move || {
            let Ok(mut signals) =
                signal_hook::iterator::Signals::new([signal_hook::consts::SIGWINCH])
            else {
                return;
            };
            let mut last = (cols, rows);
            for _ in &mut signals {
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
                // we don't know what state the screen is in. Also blanket-disable
                // the input modes the app may have switched on (mouse reporting,
                // bracketed paste, application keypad/cursor): the app is paused
                // but the real terminal would keep them, and e.g. tmux's mouse
                // mode turns every click into escape-sequence garbage typed over
                // the cooked-mode notice.
                let _ = out.write_all(b"\x1b[0m\x1b[?25h\x1b[2J\x1b[H");
                let _ = out.write_all(INPUT_MODES_OFF);
                let _ = write!(out, "\x1b[33mshellglass: {msg}\x1b[0m\r\n");
                let _ = out.flush();
            }
            Some(Msg::HubUp) if !connected => {
                connected = true;
                raw.enter();
                // Repaint the (now up-to-date) screen over the notice text, and
                // restore the input modes to whatever the app has enabled *now* —
                // the parser kept processing while paused, so this re-arms mouse
                // reporting etc. even if the app changed modes mid-outage.
                let _ = out.write_all(b"\x1b[2J\x1b[H");
                let _ = out.write_all(&parser.screen().contents_formatted());
                let _ = out.write_all(&parser.screen().input_mode_formatted());
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

/// Our controlling terminal's size as (cols, rows), if stdin is a tty.
fn term_size() -> Option<(u16, u16)> {
    let ws = rustix::termios::tcgetwinsize(std::io::stdin()).ok()?;
    (ws.ws_col > 0).then_some((ws.ws_col, ws.ws_row))
}

/// Owns the terminal's raw-mode state: `acquire` enters raw and remembers the
/// original settings; `leave`/`enter` toggle between them for the hub-outage pause.
struct RawMode {
    orig: Option<rustix::termios::Termios>,
}

impl RawMode {
    fn acquire() -> RawMode {
        // tcgetattr fails on a non-tty (e.g. piped) — leave the fd as-is.
        let Ok(orig) = rustix::termios::tcgetattr(std::io::stdin()) else {
            return RawMode { orig: None };
        };
        let mut rawt = orig.clone();
        rawt.make_raw();
        let _ = rustix::termios::tcsetattr(
            std::io::stdin(),
            rustix::termios::OptionalActions::Now,
            &rawt,
        );
        RawMode { orig: Some(orig) }
    }

    /// Restore the terminal's original (cooked) settings.
    fn leave(&self) {
        if let Some(orig) = &self.orig {
            let _ = rustix::termios::tcsetattr(
                std::io::stdin(),
                rustix::termios::OptionalActions::Now,
                orig,
            );
        }
    }

    /// Re-enter raw mode (from the saved original settings).
    fn enter(&self) {
        if let Some(orig) = &self.orig {
            let mut rawt = orig.clone();
            rawt.make_raw();
            let _ = rustix::termios::tcsetattr(
                std::io::stdin(),
                rustix::termios::OptionalActions::Now,
                &rawt,
            );
        }
    }
}

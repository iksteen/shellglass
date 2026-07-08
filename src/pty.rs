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

use crate::images::{Interceptor, Segment};
use crate::model::{Frame, ImagePlacement};
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
    let geom = term_geom().unwrap_or(TermGeom {
        cols: 80,
        rows: 24,
        px_w: 80 * FALLBACK_CELL.0,
        px_h: 24 * FALLBACK_CELL.1,
    });
    let (cols, rows) = (geom.cols, geom.rows);
    let pair = native_pty_system()
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: geom.px_w,
            pixel_height: geom.px_h,
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
    // Ask the terminal which image protocols it renders (before the input bridge
    // starts, so its replies don't leak to the child). We only intercept protocols
    // the terminal supports, so the web mirror matches what's on the local screen
    // rather than eating a sequence into a web image the terminal never showed.
    let caps = probe_caps();
    let intercept = (caps.kitty, iterm_supported(), caps.sixel);
    // Clear the local terminal so the mirrored session starts from a blank screen,
    // matching the fresh (blank) parser that viewers see (also wipes any handshake
    // reply artifacts).
    {
        use std::io::Write;
        let mut out = std::io::stdout();
        let _ = out.write_all(b"\x1b[2J\x1b[H");
        let _ = out.flush();
    }
    let (msg_tx, msg_rx) = mpsc::channel::<Msg>();
    let (frame_tx, frame_rx) = watch::channel(frame_from(&new_parser(rows, cols), &mut Vec::new()));

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
                match term_geom() {
                    Some(g) if (g.cols, g.rows) != last => {
                        last = (g.cols, g.rows);
                        let _ = master.resize(PtySize {
                            rows: g.rows,
                            cols: g.cols,
                            pixel_width: g.px_w,
                            pixel_height: g.px_h,
                        });
                        if msg_tx.send(Msg::Resize(g.rows, g.cols)).is_err() {
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
    // Pixel size of one cell, to size natural (no cell-hint) images. Roughly
    // constant across resizes, so the initial value is kept for the session.
    let cell = (
        (geom.px_w / geom.cols.max(1)).max(1),
        (geom.px_h / geom.rows.max(1)).max(1),
    );
    std::thread::spawn(move || {
        screen_thread(
            msg_rx,
            frame_tx,
            raw,
            new_parser(rows, cols),
            cell,
            intercept,
        );
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
    cell: (u16, u16),
    intercept: (bool, bool, bool),
) {
    let mut out = std::io::stdout();
    let mut connected = true; // teeing shell output to the terminal
    let mut last_frame = Instant::now();
    let mut dirty = false;
    // Inline images live outside vt100 (it drops the sequences). The interceptor
    // pulls them from the byte stream; we place each at the cursor and write a
    // private-use sentinel glyph into the parser grid at its top-left. That cell
    // then rides vt100's own scrolling/eviction/reflow, so each frame we just read
    // the sentinel's position back (see `resolve_images`) — no scroll heuristics.
    let mut interceptor = Interceptor::with(intercept.0, intercept.1, intercept.2);
    let mut images: Vec<Placed> = Vec::new();
    let mut mark_seq: u32 = 0;
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
                // Tee the *raw* stream to the local terminal first — it renders
                // sixel/kitty/iTerm2 natively, so the operator keeps seeing images.
                if connected {
                    let _ = out.write_all(&b); // immediate, not rate-limited
                    let _ = out.flush();
                }
                // vt100 only sees non-image bytes; images become placements at the
                // cursor position reached after the preceding text in this chunk.
                for seg in interceptor.feed(&b) {
                    match seg {
                        Segment::Pass(bytes) => parser.process(&bytes),
                        Segment::Image(img) => {
                            let (row, col) = parser.screen().cursor_position();
                            // App-given cell size, else derived from pixel size ÷ cell
                            // size so a natural-size image still advances the cursor.
                            let cells = img.cells.or_else(|| {
                                img.px.map(|(w, h)| {
                                    (
                                        (w.div_ceil(u32::from(cell.0)) as u16).max(1),
                                        (h.div_ceil(u32::from(cell.1)) as u16).max(1),
                                    )
                                })
                            });
                            let (cols, rows) =
                                cells.map_or((None, None), |(c, r)| (Some(c), Some(r)));
                            // Advance the parser's cursor onto the image's *last* row
                            // (the sentinel goes here); we step one line further below
                            // after placing it. `\r` first so the column resets.
                            if let Some(h) = rows {
                                parser.process(b"\r");
                                parser.process(&vec![b'\n'; h.saturating_sub(1) as usize]);
                            }
                            // Sentinel glyph at the image's *bottom*-left, so vt100
                            // tracks the cell as it scrolls/reflows. Anchoring at the
                            // bottom (not top) lets the image keep clipping as it scrolls
                            // off the top — it's only evicted once even the last row is
                            // gone. `\x1b[{col+1}G` puts it back under the image's left
                            // column (CHA is 1-based). Unique per live image (the
                            // private-use area is 6400 codepoints; rotate).
                            let mark = char::from_u32(0xE000 + mark_seq % 6400).unwrap();
                            mark_seq = mark_seq.wrapping_add(1);
                            parser.process(format!("\x1b[{}G", col + 1).as_bytes());
                            parser.process(mark.encode_utf8(&mut [0u8; 4]).as_bytes());
                            // Reset the column, then — when the image has height — step
                            // onto the line *below* it, where a sixel-scrolling terminal
                            // leaves the cursor. Landing below (not on the sentinel's row)
                            // is what keeps the image alive: a raw `cat image.sixel` adds
                            // no trailing newline, so the shell repaints its prompt
                            // immediately; if the cursor stayed on the sentinel's row that
                            // repaint would overwrite the sentinel and `resolve_images`
                            // would evict the image. An emitter that *does* add a trailing
                            // newline (chafa) then lands one line further, as in the
                            // terminal.
                            parser.process(b"\r");
                            if rows.is_some() {
                                parser.process(b"\n");
                            }
                            images.push(Placed {
                                mark,
                                img: ImagePlacement {
                                    row: i16::try_from(row).unwrap_or(0),
                                    col,
                                    cols,
                                    rows,
                                    mime: img.mime,
                                    data: img.base64,
                                },
                            });
                        }
                    }
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
            let _ = frame_tx.send(frame_from(&parser, &mut images));
            dirty = false;
            last_frame = Instant::now();
        }
    }
}

fn new_parser(rows: u16, cols: u16) -> vt100::Parser {
    vt100::Parser::new(rows, cols, 0)
}

/// An inline image plus its grid sentinel. The sentinel — a private-use glyph
/// written into the parser at the image's top-left — rides the vt100 grid, so
/// scrolling, eviction, and reflow are tracked by the parser, not guessed.
struct Placed {
    mark: char,
    img: ImagePlacement,
}

/// Snapshot the PTY screen as a [`Frame`], resolving each image's sentinel to its
/// current cell and dropping images whose sentinel is gone (scrolled off the top,
/// cleared, or overwritten).
fn frame_from(parser: &vt100::Parser, images: &mut Vec<Placed>) -> Arc<Frame> {
    let mut grid = crate::parse::grid_from_screen(parser.screen());
    resolve_images(&mut grid, images);
    Arc::new(Frame::Screen(grid))
}

/// For each tracked image, find its sentinel in the grid → that's its *bottom*-left
/// now; the top row is that minus the image height (negative once it's partially
/// scrolled off the top, so the viewer clips it). Blank the sentinel cell so it
/// never renders (the overlay covers it, but a transparent image would otherwise
/// show it). Drop images with no sentinel left (fully scrolled off, or cleared).
fn resolve_images(grid: &mut crate::model::Grid, images: &mut Vec<Placed>) {
    images.retain_mut(|p| {
        for (r, row) in grid.rows.iter_mut().enumerate() {
            for (c, cell) in row.iter_mut().enumerate() {
                if cell.text.starts_with(p.mark) {
                    let h = p.img.rows.unwrap_or(1).max(1);
                    p.img.row = r as i16 - i16::try_from(h - 1).unwrap_or(0);
                    p.img.col = c as u16;
                    *cell = crate::model::StyledCell::default(); // scrub
                    return true;
                }
            }
        }
        false // sentinel gone → evict
    });
    grid.images = images.iter().map(|p| p.img.clone()).collect();
}

/// Controlling-terminal geometry: cell counts plus the PTY's pixel dimensions.
/// Pixel-aware apps (kitty/sixel image tools) refuse to draw unless the terminal
/// reports a non-zero pixel size, so we pass through the outer terminal's reported
/// pixels and, when it reports none, synthesize them from an assumed cell size.
struct TermGeom {
    cols: u16,
    rows: u16,
    px_w: u16,
    px_h: u16,
}

/// Assumed cell size when the outer terminal reports no pixel dimensions. The
/// browser rescales each image to its cell box regardless, so this only sets the
/// source resolution a tool picks — a sane default, not a measurement.
// ponytail: bump if graphics come out mis-scaled on terminals that report 0 pixels.
const FALLBACK_CELL: (u16, u16) = (8, 16);

/// Our controlling terminal's geometry, if stdin is a tty.
fn term_geom() -> Option<TermGeom> {
    let ws = rustix::termios::tcgetwinsize(std::io::stdin()).ok()?;
    if ws.ws_col == 0 {
        return None;
    }
    let px = |reported: u16, cells: u16, cell: u16| {
        if reported > 0 {
            reported
        } else {
            cells.saturating_mul(cell)
        }
    };
    Some(TermGeom {
        cols: ws.ws_col,
        rows: ws.ws_row,
        px_w: px(ws.ws_xpixel, ws.ws_col, FALLBACK_CELL.0),
        px_h: px(ws.ws_ypixel, ws.ws_row, FALLBACK_CELL.1),
    })
}

/// Graphics-protocol support the controlling terminal advertises, learned from a
/// capability handshake rather than a `TERM` signature.
#[derive(Clone, Copy, Default)]
struct Caps {
    /// Kitty graphics — the `a=q` query drew an `OK` response.
    kitty: bool,
    /// Sixel — Primary DA listed feature `4`.
    sixel: bool,
}

/// Ask the terminal which image protocols it renders. Emits a kitty graphics
/// support query then Primary DA; DA is the fence (every terminal answers it, so
/// its reply ends the wait — no fixed timeout to guess). Returns nothing if stdin
/// isn't a tty or the terminal stays silent. Must run before the stdin→PTY bridge
/// starts, so the replies are consumed here and not forwarded to the child.
fn probe_caps() -> Caps {
    use rustix::termios::{OptionalActions, SpecialCodeIndex, tcgetattr, tcsetattr};
    use std::os::fd::AsFd;
    let stdin = std::io::stdin();
    let fd = stdin.as_fd();
    let Ok(saved) = tcgetattr(fd) else {
        return Caps::default(); // not a tty
    };
    // Read with a 0.1s-per-read timeout (VMIN=0/VTIME=1) so a silent terminal can't
    // hang startup; restore the raw settings afterward.
    let mut probe = saved.clone();
    probe.special_codes[SpecialCodeIndex::VMIN] = 0;
    probe.special_codes[SpecialCodeIndex::VTIME] = 1;
    if tcsetattr(fd, OptionalActions::Now, &probe).is_err() {
        return Caps::default();
    }
    let _ = rustix::io::write(
        std::io::stdout().as_fd(),
        b"\x1b_Gi=1,a=q,s=1,v=1,t=d,f=24;AAAA\x1b\\\x1b[c",
    );
    let mut buf = Vec::new();
    let mut chunk = [0u8; 256];
    // A real terminal answers DA in milliseconds and we break the instant it does
    // (VTIME returns on first byte, it doesn't wait out the tick). This 0.5s cap
    // (5 × 0.1s) only bounds the pathological "tty that never answers DA" — a bare
    // pty, not a real terminal — while still covering a slow ssh round-trip.
    for _ in 0..5 {
        match rustix::io::read(fd, &mut chunk) {
            Ok(0) => {} // timeout tick, keep waiting
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
        if da_seen(&buf) {
            break;
        }
    }
    let _ = tcsetattr(fd, OptionalActions::Now, &saved);
    parse_caps(&buf)
}

/// Terminals that render the iTerm2 inline-image protocol. Its OSC 1337 has no
/// capability query (unlike kitty graphics / sixel), so this is the one protocol we
/// can only detect by a `TERM_PROGRAM` signature — a deliberate, documented
/// exception to the query-don't-sniff rule.
// ponytail: extend the list as other terminals adopt the iTerm2 protocol.
fn iterm_supported() -> bool {
    matches!(
        std::env::var("TERM_PROGRAM").as_deref(),
        Ok("iTerm.app" | "WezTerm" | "vscode" | "mintty" | "Hyper" | "rio")
    )
}

/// A Primary DA reply (`ESC [ ? … c`) has arrived — the handshake fence.
fn da_seen(buf: &[u8]) -> bool {
    find(buf, b"\x1b[?").is_some_and(|p| find(&buf[p + 3..], b"c").is_some())
}

/// Interpret the collected handshake replies.
fn parse_caps(buf: &[u8]) -> Caps {
    Caps {
        kitty: kitty_ok(buf),
        sixel: da_sixel(buf),
    }
}

/// A kitty graphics APC reply (`ESC _ G … ; OK … ST`) confirms support.
fn kitty_ok(buf: &[u8]) -> bool {
    let mut i = 0;
    while let Some(p) = find(&buf[i..], b"\x1b_G") {
        let start = i + p + 3;
        let end = find(&buf[start..], b"\x1b\\").map_or(buf.len(), |e| start + e);
        if find(&buf[start..end], b";OK").is_some() {
            return true;
        }
        i = end;
    }
    false
}

/// The Primary DA feature list includes `4` (sixel).
fn da_sixel(buf: &[u8]) -> bool {
    let Some(p) = find(buf, b"\x1b[?") else {
        return false;
    };
    let params = &buf[p + 3..];
    let end = params
        .iter()
        .position(|&b| b == b'c')
        .unwrap_or(params.len());
    params[..end].split(|&b| b == b';').any(|f| f == b"4")
}

/// Byte-substring search (needle non-empty).
fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handshake_replies_parse_to_caps() {
        // kitty OK + a DA that lists sixel (4).
        let both = b"\x1b_Gi=1;OK\x1b\\\x1b[?62;4;22c";
        let caps = parse_caps(both);
        assert!(caps.kitty && caps.sixel);
        assert!(da_seen(both));

        // DA without 4, and a kitty *error* reply → neither.
        let neither = b"\x1b_Gi=1;ENOTSUPPORTED:nope\x1b\\\x1b[?62;22c";
        let caps = parse_caps(neither);
        assert!(!caps.kitty && !caps.sixel);

        // No DA yet → fence hasn't arrived.
        assert!(!da_seen(b"\x1b_Gi=1;OK\x1b\\"));
    }

    // A 2-row-tall image; its sentinel marks the *bottom* row.
    fn placed(mark: char) -> Placed {
        Placed {
            mark,
            img: ImagePlacement {
                row: 0,
                col: 0,
                cols: Some(2),
                rows: Some(2),
                mime: "image/png".into(),
                data: String::new(),
            },
        }
    }

    /// The bottom sentinel rides vt100's own scrolling: the reported top row falls
    /// as the screen scrolls, goes negative while the image clips against the top
    /// edge, and the image is only evicted once even its bottom row is gone.
    #[test]
    fn sentinel_tracks_scroll_clips_then_evicts() {
        let mut parser = new_parser(3, 10); // 3 rows
        let mark = '\u{E000}';
        parser.process(b"\r\n\r\n"); // cursor to the last row
        parser.process(mark.encode_utf8(&mut [0u8; 4]).as_bytes());
        let mut imgs = vec![placed(mark)];

        // Bottom sentinel at row 2, height 2 → top row 1. Sentinel cell is scrubbed.
        let Frame::Screen(g) = &*frame_from(&parser, &mut imgs) else {
            panic!("screen")
        };
        assert_eq!((g.images[0].row, g.images[0].col), (1, 0));
        assert!(g.rows[2].first().is_none_or(|c| c.text.is_empty()));

        // One scroll lifts the sentinel to row 1 → top row 0.
        parser.process(b"\r\nx");
        let Frame::Screen(g) = &*frame_from(&parser, &mut imgs) else {
            panic!("screen")
        };
        assert_eq!(g.images[0].row, 0);

        // Another scroll: sentinel at row 0 → top row -1, image still shown (clipped).
        parser.process(b"\r\ny");
        let Frame::Screen(g) = &*frame_from(&parser, &mut imgs) else {
            panic!("screen")
        };
        assert_eq!(g.images[0].row, -1);

        // One more: the bottom row is gone too → the image is evicted.
        parser.process(b"\r\nz");
        let Frame::Screen(g) = &*frame_from(&parser, &mut imgs) else {
            panic!("screen")
        };
        assert!(g.images.is_empty());
    }
}

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
        // The real session parser records its blind spots for the exit report
        // (`new_parser`'s throwaway SeqLog handles the tests/initial frame).
        let (seqlog, seq_seen) = SeqLog::new();
        let parser = vt100::Parser::new_with_callbacks(rows, cols, 0, seqlog);
        screen_thread(msg_rx, frame_tx, raw, parser, seq_seen, cell, intercept);
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
    mut parser: vt100::Parser<SeqLog>,
    seq_seen: SeqSeen,
    cell: (u16, u16),
    intercept: (bool, bool, bool),
) {
    let mut out = std::io::stdout();
    let mut connected = true; // teeing shell output to the terminal
    let mut last_frame = Instant::now();
    let mut dirty = false;
    // Inline images live outside vt100 (it drops the sequences). The interceptor
    // pulls them from the byte stream; we place each at the cursor and write
    // sentinel glyphs into the parser grid at the image's corners. Those cells
    // then ride vt100's own scrolling/eviction/reflow, so each frame we just read
    // the sentinels' positions back (see `resolve_images`) — no scroll heuristics.
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
                            // Track the image by sampling its four corners with sentinel
                            // glyphs written into the parser grid, so vt100 handles
                            // scroll/reflow/eviction for us (`resolve_images` reads them
                            // back). Corner sampling approximates a cell-based sixel
                            // terminal's own erase semantics: text written over an image
                            // cell erases that portion of the image, so a sentinel dying
                            // means the terminal erased that corner too. Any surviving
                            // corner reconstructs the top-left via its stored offset;
                            // the image is evicted only once *every* corner is gone —
                            // i.e. once the terminal has erased it wholesale (full
                            // repaint, clear, scrolled fully off). Concretely:
                            //   • a prompt repainted after a raw `cat image.sixel` (no
                            //     trailing newline) kills the bottom-left corner — the
                            //     other three keep the image alive, even at 1 row tall
                            //     (the top-right/bottom-right corner outlives any prompt
                            //     shorter than the image is wide);
                            //   • scrolling off the top removes the top corners — the
                            //     bottom ones reconstruct a negative row, so the viewer
                            //     keeps clipping until even the last row is gone.
                            // ponytail: corners only — an interior overwrite doesn't
                            // punch a hole in the overlay; per-cell erase mirroring would
                            // need viewer-side clip regions.
                            // Sentinels come from Plane-16 PUA (U+100000+), which real
                            // terminal content never carries — unlike U+E000 BMP PUA,
                            // where Nerd Font / Powerline glyphs would false-match.
                            let (_, screen_cols) = parser.screen().size();
                            // Rightmost on-screen column of the image (clamped to the
                            // grid edge, like the terminal clips a too-wide sixel); no
                            // right corners for 1-cell-wide or unknown-size images.
                            let right = cols
                                .map(|w| {
                                    let rc = (u32::from(col) + u32::from(w) - 1)
                                        .min(u32::from(screen_cols.max(1)) - 1);
                                    u16::try_from(rc).unwrap_or(col)
                                })
                                .filter(|&rc| rc > col);
                            let mut marks = Vec::with_capacity(4);
                            marks.push((drop_mark(&mut parser, &mut mark_seq, col), 0, 0));
                            if let Some(rc) = right {
                                marks.push((
                                    drop_mark(&mut parser, &mut mark_seq, rc),
                                    0,
                                    rc - col,
                                ));
                            }
                            // Advance onto the image's last row and drop the bottom
                            // corners; leave the cursor there (col 0), which is where a
                            // sixel-scrolling terminal leaves it — an emitter's own
                            // trailing newline (chafa) then lands one line below,
                            // matching the terminal exactly.
                            if let Some(h) = rows {
                                parser.process(b"\r");
                                parser.process(&vec![b'\n'; usize::from(h.saturating_sub(1))]);
                                if h > 1 {
                                    marks.push((
                                        drop_mark(&mut parser, &mut mark_seq, col),
                                        h - 1,
                                        0,
                                    ));
                                    if let Some(rc) = right {
                                        marks.push((
                                            drop_mark(&mut parser, &mut mark_seq, rc),
                                            h - 1,
                                            rc - col,
                                        ));
                                    }
                                }
                            }
                            parser.process(b"\r");
                            images.push(Placed {
                                marks,
                                img: ImagePlacement {
                                    row: i16::try_from(row).unwrap_or(0),
                                    col,
                                    cols,
                                    rows,
                                    mime: img.mime,
                                    data: img.base64.into(),
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
                // The terminal is cooked again — the one moment printing is safe.
                report_unmirrored(&seq_seen);
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

fn new_parser(rows: u16, cols: u16) -> vt100::Parser<SeqLog> {
    vt100::Parser::new_with_callbacks(rows, cols, 0, SeqLog::new().0)
}

/// The parser's blind spots, recorded for the exit report. vt100 silently drops
/// escape sequences it doesn't implement, and each dropped kind is a potential
/// mirror-fidelity gap — the local terminal may render what the browser doesn't
/// (the SCOSC/SCORC bug class). In serve/push mode we OWN the terminal (raw
/// mode and tee), so nothing may be printed while the session runs: kinds are
/// deduplicated in memory (bounded by [`SeqLog::MAX_KINDS`]) and reported to
/// stderr once at shutdown, after raw mode is restored. Set
/// `SHELLGLASS_SEQ_LOG=<path>` to also append each kind as it is first seen —
/// a file, never the tty — when debugging a long-lived session.
pub struct SeqLog {
    seen: Arc<std::sync::Mutex<std::collections::BTreeSet<String>>>,
    file: Option<std::fs::File>,
}

/// Shared view of the recorded kinds, for the shutdown report.
type SeqSeen = Arc<std::sync::Mutex<std::collections::BTreeSet<String>>>;

impl SeqLog {
    /// Cap on distinct recorded kinds: binary garbage `cat`ed to the terminal
    /// must not grow memory; whatever real gaps exist will land well before it.
    const MAX_KINDS: usize = 64;

    fn new() -> (SeqLog, SeqSeen) {
        let seen: SeqSeen = Arc::default();
        let file = std::env::var_os("SHELLGLASS_SEQ_LOG").and_then(|p| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)
                .ok()
        });
        (
            SeqLog {
                seen: seen.clone(),
                file,
            },
            seen,
        )
    }

    fn record(&mut self, kind: String) {
        let mut seen = self.seen.lock().unwrap();
        if seen.len() >= Self::MAX_KINDS || !seen.insert(kind.clone()) {
            return;
        }
        drop(seen);
        if let Some(f) = &mut self.file {
            let _ = writeln!(f, "{kind}");
        }
    }
}

/// One short descriptor per CSI shape. Mode set/reset (`h`/`l`) and SGR (`m`)
/// gaps are per-parameter, so their params join the key; for everything else
/// the intermediates + final byte are the kind.
fn csi_kind(i1: Option<u8>, i2: Option<u8>, params: &[&[u16]], c: char) -> String {
    let mut s = String::from("CSI");
    for i in [i1, i2].into_iter().flatten() {
        s.push(' ');
        s.push(char::from(i));
    }
    if matches!(c, 'h' | 'l' | 'm') {
        for p in params {
            s.push(' ');
            let sub: Vec<String> = p.iter().map(u16::to_string).collect();
            s.push_str(&sub.join(":"));
        }
    }
    s.push(' ');
    s.push(c);
    s
}

impl vt100::Callbacks for SeqLog {
    fn unhandled_char(&mut self, _: &mut vt100::Screen, c: char) {
        // U+FFFD is decode noise from binary output, not a sequence gap.
        if c != '\u{fffd}' {
            self.record(format!("CHAR U+{:04X}", u32::from(c)));
        }
    }
    fn unhandled_control(&mut self, _: &mut vt100::Screen, b: u8) {
        self.record(format!("CTRL 0x{b:02x}"));
    }
    fn unhandled_escape(&mut self, _: &mut vt100::Screen, i1: Option<u8>, i2: Option<u8>, b: u8) {
        let mut s = String::from("ESC");
        for i in [i1, i2].into_iter().flatten() {
            s.push(' ');
            s.push(char::from(i));
        }
        s.push(' ');
        s.push(char::from(b));
        self.record(s);
    }
    fn unhandled_csi(
        &mut self,
        _: &mut vt100::Screen,
        i1: Option<u8>,
        i2: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        self.record(csi_kind(i1, i2, params, c));
    }
    fn unhandled_osc(&mut self, _: &mut vt100::Screen, params: &[&[u8]]) {
        let selector = params
            .first()
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .unwrap_or_default();
        self.record(format!("OSC {selector}"));
    }
}

/// After raw mode is restored (and ONLY then — never mid-session, we own the
/// screen), tell the operator which escape sequences the mirror couldn't render.
fn report_unmirrored(seen: &SeqSeen) {
    let seen = seen.lock().unwrap();
    if seen.is_empty() {
        return;
    }
    let kinds: Vec<&str> = seen.iter().map(String::as_str).collect();
    eprintln!(
        "shellglass: {} escape sequence kind(s) were not mirrored (the local \
         terminal may have rendered more than the browser): {} — please report",
        seen.len(),
        kinds.join(", ")
    );
}

/// An inline image plus its corner sentinels — Plane-16 private-use glyphs written
/// into the parser at up to four corners of the image, each remembering its offset
/// from the top-left as `(glyph, rows_below_top, cols_right_of_left)`. They ride
/// the vt100 grid, so scrolling, eviction, and reflow are tracked by the parser,
/// not guessed; any surviving corner reconstructs the image position, and the
/// image is evicted only once all of them are gone (see placement for why that
/// mirrors a sixel terminal's erase semantics).
struct Placed {
    marks: Vec<(char, u16, u16)>,
    img: ImagePlacement,
}

/// Write one sentinel glyph at `at_col` on the parser's current row (CHA is
/// 1-based) and return it. Sentinels rotate through Plane-16 PUA.
fn drop_mark(parser: &mut vt100::Parser<SeqLog>, mark_seq: &mut u32, at_col: u16) -> char {
    let m = char::from_u32(0x10_0000 + *mark_seq % 0xFFFE).unwrap();
    *mark_seq = mark_seq.wrapping_add(1);
    parser.process(format!("\x1b[{}G", at_col + 1).as_bytes());
    parser.process(m.encode_utf8(&mut [0u8; 4]).as_bytes());
    m
}

/// Snapshot the PTY screen as a [`Frame`], resolving each image's sentinel to its
/// current cell and dropping images whose sentinel is gone (scrolled off the top,
/// cleared, or overwritten).
fn frame_from(parser: &vt100::Parser<SeqLog>, images: &mut Vec<Placed>) -> Arc<Frame> {
    let mut grid = crate::parse::grid_from_screen(parser.screen());
    resolve_images(&mut grid, images);
    Arc::new(Frame::Screen(grid))
}

/// For each tracked image, locate its surviving corner sentinels in the grid and
/// reconstruct the top-left from the best one (placement order: top-left,
/// top-right, bottom-left, bottom-right — a bottom corner reconstructs a negative
/// row once the image has partially scrolled off the top, so the viewer clips it).
/// Blank every sentinel cell found so it never renders (the overlay covers it, but
/// a transparent image would otherwise show it). Drop images with no corner left —
/// fully scrolled off, cleared, or wholly overwritten.
fn resolve_images(grid: &mut crate::model::Grid, images: &mut Vec<Placed>) {
    // One pass over the grid (not one per image): find and scrub every tracked
    // sentinel. The row vec omits wide continuation cells, so the true screen
    // column is tracked by advancing per cell *width* — a CJK/emoji prompt before
    // a sentinel on its row must not drag the reconstructed column left, or the
    // browser draws the overlay shifted onto the preceding text.
    let mut hits: Vec<(char, i16, u16)> = Vec::new();
    for (r, row) in grid.rows.iter_mut().enumerate() {
        let mut col: u16 = 0;
        for cell in row.iter_mut() {
            let width = if cell.wide { 2 } else { 1 };
            if let Some(ch) = cell.text.chars().next() {
                // Sentinels live in Plane-16 PUA — cheap pre-filter, then confirm
                // the glyph belongs to a tracked image before scrubbing it.
                if ch >= '\u{100000}'
                    && images
                        .iter()
                        .any(|p| p.marks.iter().any(|&(m, _, _)| m == ch))
                {
                    hits.push((ch, r as i16, col));
                    *cell = crate::model::StyledCell::default(); // scrub
                }
            }
            col += width;
        }
    }
    images.retain_mut(|p| {
        let mut found: Option<(i16, u16)> = None; // reconstructed top-left
        let mut best = usize::MAX;
        for &(ch, r, c) in &hits {
            if let Some(i) = p.marks.iter().position(|&(m, _, _)| m == ch)
                && i < best
            {
                let (_, dr, dc) = p.marks[i];
                best = i;
                found = Some((
                    r - i16::try_from(dr).unwrap_or(i16::MAX),
                    c.saturating_sub(dc),
                ));
            }
        }
        if let Some((row, col)) = found {
            p.img.row = row;
            p.img.col = col;
            true
        } else {
            false // every corner gone → evict
        }
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
    // (VTIME returns on first byte, it doesn't wait out the tick), so the deadline
    // only bounds the pathological "tty that never answers DA" — a bare pty, not
    // a real terminal. It must err generous: a reply arriving *after* we stop
    // draining is forwarded to the child shell as typed input (ESC-prefixed
    // garbage at the prompt) and capability detection silently fails. 2s covers
    // slow ssh round-trips; beyond that the residual risk is accepted.
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
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

    // Feed sequences vt100 doesn't implement; the parser's SeqLog must record
    // each kind once, and only the kinds actually seen.
    #[test]
    fn seqlog_records_unhandled_kinds_once() {
        let (seqlog, seen) = SeqLog::new();
        let mut parser = vt100::Parser::new_with_callbacks(24, 80, 0, seqlog);
        // DECSCA (CSI " q), a made-up DEC private mode, and a handled sequence
        // (CUP) that must NOT be recorded — twice over to check dedup. (The
        // original specimens here, DECSTR and mode 2026, got implemented off
        // the back of this very telemetry — pick obscure ones.)
        for _ in 0..2 {
            parser.process(b"\x1b[\"q\x1b[?1234h\x1b[5;5H");
        }
        let seen = seen.lock().unwrap();
        assert_eq!(
            seen.iter().cloned().collect::<Vec<_>>(),
            vec!["CSI \" q".to_string(), "CSI ? 1234 h".to_string()]
        );
    }

    #[test]
    fn seqlog_kind_cap_bounds_memory() {
        let (mut seqlog, seen) = SeqLog::new();
        for i in 0..(SeqLog::MAX_KINDS + 50) {
            seqlog.record(format!("OSC {i}"));
        }
        assert_eq!(seen.lock().unwrap().len(), SeqLog::MAX_KINDS);
    }

    // Pins the vendored vt100's SCOSC/SCORC patch from the consumer side: a
    // powerline-style prompt draws its right-aligned git segment as save →
    // jump right → draw → restore, and the mirrored cursor must land back
    // after the left prompt (kitty restores it; the stock 0.16.2 parser
    // ignored CSI s/u and left it at the right edge, corrupting the layout on
    // every keystroke).
    #[test]
    fn right_aligned_prompt_restores_cursor() {
        let mut parser = new_parser(24, 80);
        parser.process(b"$ ls\x1b[s\x1b[80C\x1b[11D\x1b[7m  master \x1b[0m\x1b[u");
        assert_eq!(parser.screen().cursor_position(), (0, 4));
    }

    fn img_2x2() -> ImagePlacement {
        ImagePlacement {
            row: 0,
            col: 0,
            cols: Some(2),
            rows: Some(2),
            mime: "image/png".into(),
            data: "".into(),
        }
    }

    // A 2-row-tall image tracked only by its bottom-left corner (the top corner's
    // glyph is never written to the grid), exercising the clip-as-it-scrolls
    // fallback path.
    fn placed(mark: char) -> Placed {
        Placed {
            marks: vec![('\u{10FFF0}', 0, 0), (mark, 1, 0)],
            img: img_2x2(),
        }
    }

    /// The bottom sentinel rides vt100's own scrolling: the reported top row falls
    /// as the screen scrolls, goes negative while the image clips against the top
    /// edge, and the image is only evicted once even its bottom row is gone.
    #[test]
    fn sentinel_tracks_scroll_clips_then_evicts() {
        let mut parser = new_parser(3, 10); // 3 rows
        let mark = '\u{100003}'; // sentinels are always Plane-16 PUA (see drop_mark)
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

    // A surviving top corner keeps the image alive when the bottom-left corner is
    // overwritten in place — e.g. a shell prompt repainted right after a raw
    // `cat image.sixel`, which adds no trailing newline. The image reports its top
    // row (reconstructed from the top corner) and is not evicted.
    #[test]
    fn top_corner_survives_bottom_overwrite() {
        let (top, bottom) = ('\u{100000}', '\u{100001}');
        let mut parser = new_parser(4, 10);
        // top corner on row 0, bottom corner on row 1 (a 2-row image).
        parser.process(top.encode_utf8(&mut [0u8; 4]).as_bytes());
        parser.process(b"\r\n");
        parser.process(bottom.encode_utf8(&mut [0u8; 4]).as_bytes());
        parser.process(b"\r"); // cursor at col 0 of the bottom row (as after placement)
        let mut imgs = vec![Placed {
            marks: vec![(top, 0, 0), (bottom, 1, 0)],
            img: img_2x2(),
        }];

        // A prompt repaints the bottom corner's row, clobbering that sentinel.
        parser.process(b"user@host$ ");
        let Frame::Screen(g) = &*frame_from(&parser, &mut imgs) else {
            panic!("screen")
        };
        // Still tracked, via the top corner, at its true top row.
        assert_eq!(g.images.len(), 1);
        assert_eq!((g.images[0].row, g.images[0].col), (0, 0));
    }

    // A 1-row image wider than the prompt survives via its top-right corner: the
    // prompt erases the left corner (and, in the terminal, that part of the
    // image), the right corner reconstructs the original top-left.
    #[test]
    fn one_row_image_survives_prompt_via_right_corner() {
        let (left, right) = ('\u{100000}', '\u{100001}');
        let mut parser = new_parser(4, 30);
        // a 20-cell-wide, 1-row image at col 0: corners at cols 0 and 19.
        parser.process(left.encode_utf8(&mut [0u8; 4]).as_bytes());
        parser.process(b"\x1b[20G");
        parser.process(right.encode_utf8(&mut [0u8; 4]).as_bytes());
        parser.process(b"\r");
        let mut imgs = vec![Placed {
            marks: vec![(left, 0, 0), (right, 0, 19)],
            img: ImagePlacement {
                row: 0,
                col: 0,
                cols: Some(20),
                rows: Some(1),
                mime: "image/png".into(),
                data: "".into(),
            },
        }];

        // The prompt covers cols 0..11 — the left corner dies, the right survives.
        parser.process(b"user@host$ ");
        let Frame::Screen(g) = &*frame_from(&parser, &mut imgs) else {
            panic!("screen")
        };
        assert_eq!(g.images.len(), 1);
        assert_eq!((g.images[0].row, g.images[0].col), (0, 0));
    }

    // Wide glyphs occupy two screen columns but only one row-vec slot (parsing
    // drops the continuation cell) — CJK/emoji before a sentinel on its row must
    // not drag the reconstructed column left, or the browser draws the overlay
    // shifted onto the preceding text.
    #[test]
    fn sentinel_column_correct_after_wide_glyphs() {
        let mark = '\u{100000}';
        let mut parser = new_parser(4, 30);
        parser.process("日本語 ".as_bytes()); // three wide glyphs + a space → col 7
        parser.process(mark.encode_utf8(&mut [0u8; 4]).as_bytes());
        let mut imgs = vec![Placed {
            marks: vec![(mark, 0, 0)],
            img: img_2x2(),
        }];
        let Frame::Screen(g) = &*frame_from(&parser, &mut imgs) else {
            panic!("screen")
        };
        assert_eq!((g.images[0].row, g.images[0].col), (0, 7));
    }

    // A 1-row image narrower than what overwrites it is evicted — every corner is
    // erased, which is exactly when the terminal has erased the whole image too.
    #[test]
    fn fully_overwritten_image_evicts() {
        let (left, right) = ('\u{100000}', '\u{100001}');
        let mut parser = new_parser(4, 30);
        // a 5-cell-wide, 1-row image: corners at cols 0 and 4.
        parser.process(left.encode_utf8(&mut [0u8; 4]).as_bytes());
        parser.process(b"\x1b[5G");
        parser.process(right.encode_utf8(&mut [0u8; 4]).as_bytes());
        parser.process(b"\r");
        let mut imgs = vec![Placed {
            marks: vec![(left, 0, 0), (right, 0, 4)],
            img: ImagePlacement {
                row: 0,
                col: 0,
                cols: Some(5),
                rows: Some(1),
                mime: "image/png".into(),
                data: "".into(),
            },
        }];

        // An 11-char prompt paints across the entire image row.
        parser.process(b"user@host$ ");
        let Frame::Screen(g) = &*frame_from(&parser, &mut imgs) else {
            panic!("screen")
        };
        assert!(g.images.is_empty());
        assert!(imgs.is_empty());
    }
}

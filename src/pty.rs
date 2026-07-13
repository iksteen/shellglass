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

use crate::images::{Interceptor, Step};
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

/// Debounce a cursor-hidden transient for this long. Apps bracket each redraw in
/// `?25l … ?25h` (hide, repaint, show); the local terminal applies the burst as
/// one refresh so the cursor is never composited hidden, but our 30fps sampling
/// catches the sub-frame hidden gap as its own frame and the mirrored cursor
/// blinks (a spinner does this ~14×/s). Keep showing the last position until the
/// app has held the cursor hidden this long, then let it truly hide — so a
/// redraw-transient hide never ships, while a genuine hide still does, one grace
/// window late (imperceptible for a non-interactive cursor).
const CURSOR_HIDE_GRACE: Duration = Duration::from_millis(150);

/// One cell's share of an inline-image placement, stored in the vendored
/// vt100's generic per-cell data slot (`Cell<T>`): the placement id plus this
/// cell's offset within the image, so any surviving cell reconstructs the
/// placement's top-left exactly — scrolling, line edits, and erasure need no
/// extra tracking (the slot dies with the cell's contents).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ImgCell {
    id: std::num::NonZeroU32,
    row_off: u16,
    col_off: u16,
}

/// The session parser: [`SeqLog`] telemetry callbacks + [`ImgCell`] cell data.
type SgParser = vt100::Parser<SeqLog, ImgCell>;

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
    /// A deferred image's payload, decoded/encoded by the worker thread —
    /// fills the pending [`Placed`] with the same `id` (dropped if the
    /// placement was already overwritten/evicted).
    ImageReady {
        id: std::num::NonZeroU32,
        hash: String,
        blob: crate::model::ImageBlob,
    },
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
/// `sixel_compat` opts into the EXPERIMENTAL sixel→kitty/iTerm2 transcode.
pub fn start(
    command: &[String],
    sixel_compat: bool,
) -> Result<(watch::Receiver<Arc<Frame>>, Notifier)> {
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
    // Shared so BOTH the stdin bridge (keystrokes + the local terminal's own query
    // replies) and the screen thread (synthetic kitty rejections, see `Segment::Reject`)
    // can write to the app. Contention is nil — injections are rare and tiny.
    let writer: Arc<std::sync::Mutex<Box<dyn Write + Send>>> = Arc::new(std::sync::Mutex::new(
        master.take_writer().context("taking pty writer")?,
    ));

    // Raw mode now, before the child draws anything.
    let raw = RawMode::acquire();
    // Ask the terminal which image protocols it renders (before the input bridge
    // starts, so its replies don't leak to the child). We only intercept protocols
    // the terminal supports, so the web mirror matches what's on the local screen
    // rather than eating a sequence into a web image the terminal never showed.
    let caps = probe_caps();
    let iterm = iterm_supported();
    // EXPERIMENTAL (opt-in via --sixel-compat): terminal renders kitty/iTerm2
    // graphics but NOT sixel → transcode sixel into that protocol so sixel-emitting
    // tools (esp. through tmux, which carries sixel in its grid model) show up
    // locally and mirror to the web. When sixel is native, or the flag is off,
    // `transcode` is None and the fast raw path is kept untouched.
    let transcode = if !sixel_compat || caps.sixel {
        None
    } else if caps.kitty {
        Some(crate::images::GfxProto::Kitty)
    } else if iterm {
        Some(crate::images::GfxProto::Iterm)
    } else {
        None
    };
    // Intercept sixel if the terminal renders it natively OR we're transcoding it.
    let intercept = (caps.kitty, iterm, caps.sixel || transcode.is_some());
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
    let (frame_tx, frame_rx) = watch::channel(frame_from(
        &new_parser(rows, cols),
        &mut Vec::new(),
        &mut Vec::new(),
        &mut CursorBridge::default(),
    ));

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

    // Our stdin → PTY. EXPERIMENTAL: when transcoding sixel, also advertise sixel to
    // the child by adding feature `4` to the terminal's Primary DA reply as it passes
    // through — so sixel-aware tools (and tmux) actually emit sixel, which we then
    // transcode. `None` keeps the original verbatim, zero-overhead bridge.
    {
        let writer = writer.clone();
        let mut da = transcode.map(|_| DaRewriter::default());
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            let mut stdin = std::io::stdin();
            loop {
                match stdin.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let Ok(mut w) = writer.lock() else { break };
                        let ok = match &mut da {
                            Some(da) => w.write_all(&da.advertise_sixel(&buf[..n])),
                            None => w.write_all(&buf[..n]),
                        };
                        if ok.is_err() {
                            break;
                        }
                        let _ = w.flush();
                    }
                }
            }
        });
    }

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
    // The decode worker answers back through the screen thread's own channel.
    let ready_tx = msg_tx.clone();
    let inject = writer.clone();
    std::thread::spawn(move || {
        // The real session parser records its blind spots for the exit report
        // (`new_parser`'s throwaway SeqLog handles the tests/initial frame).
        let (seqlog, seq_seen) = SeqLog::new();
        let parser = vt100::Parser::new_with_callbacks(rows, cols, 0, seqlog);
        screen_thread(
            msg_rx, ready_tx, frame_tx, raw, parser, seq_seen, cell, intercept, transcode, inject,
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

#[allow(clippy::too_many_arguments)] // ponytail: one call site, private
fn screen_thread(
    msg_rx: mpsc::Receiver<Msg>,
    ready_tx: mpsc::Sender<Msg>,
    frame_tx: watch::Sender<Arc<Frame>>,
    raw: RawMode,
    mut parser: SgParser,
    seq_seen: SeqSeen,
    cell: (u16, u16),
    intercept: (bool, bool, bool),
    transcode: Option<crate::images::GfxProto>,
    inject: Arc<std::sync::Mutex<Box<dyn Write + Send>>>,
) {
    let mut out = std::io::stdout();
    let mut connected = true; // teeing shell output to the terminal
    let mut last_frame = Instant::now();
    let mut dirty = false;
    // Deferred-image decode worker: sixel/kitty-raw payloads decode + PNG-
    // encode OFF this thread, so the tee to the local terminal never stalls
    // behind a deflate (a sixel video would otherwise slow the terminal
    // itself, 4–12× measured). Newest wins: jobs superseded while one was
    // being processed are drained and dropped — their placements stay
    // pending until cell overwrite evicts them — and having skipped ANY job
    // is the backpressure signal that switches PNG encoding to the fast
    // compression level until the worker catches up.
    let (job_tx, job_rx) =
        mpsc::channel::<(std::num::NonZeroU32, crate::images::DeferredPayload)>();
    std::thread::spawn(move || {
        while let Ok(mut job) = job_rx.recv() {
            let mut superseded = false;
            while let Ok(newer) = job_rx.try_recv() {
                job = newer;
                superseded = true;
            }
            let Some((png, _px)) = crate::images::finish_deferred(&job.1, superseded) else {
                continue; // undecodable payload: the placement stays pending → evicted
            };
            let mime = "image/png".to_string();
            let hash = crate::proto::content_key(&mime, &png);
            let blob = crate::model::ImageBlob {
                mime,
                bytes: png.into(),
            };
            if ready_tx
                .send(Msg::ImageReady {
                    id: job.0,
                    hash,
                    blob,
                })
                .is_err()
            {
                break; // screen thread gone
            }
        }
    });
    // Inline images live outside vt100's byte stream (it drops the sequences).
    // The interceptor pulls them out; each is placed at the cursor by stamping
    // per-cell image tags into the parser grid (`place_data`), which then ride
    // vt100's own scrolling/eviction/reflow — each frame we just read the
    // surviving tags back (see `resolve_images`), no scroll heuristics.
    let mut interceptor = Interceptor::with(intercept.0, intercept.1, intercept.2, transcode, cell);
    let mut sync = SyncGate::new();
    let mut images: Vec<Placed> = Vec::new();
    // Ready-but-overwritten placements held for the double-buffer swap
    // (see resolve_images): shown until their pending successor's bytes land.
    let mut zombies: Vec<Placed> = Vec::new();
    let mut image_seq = std::num::NonZeroU32::MIN;
    let mut bridge = CursorBridge::default();
    loop {
        // Wake at the soonest of: the frame interval (when dirty) and the
        // cursor-hide grace expiry (when bridging a hidden cursor, so the real
        // hide still ships even if the app then goes quiet). Neither ⇒ block.
        let wait = {
            let mut d = dirty.then(|| MIN_FRAME.saturating_sub(last_frame.elapsed()));
            if let Some(t) = bridge.hidden_since.filter(|_| bridge.shown.is_some()) {
                let rem = CURSOR_HIDE_GRACE.saturating_sub(t.elapsed());
                d = Some(d.map_or(rem, |x| x.min(rem)));
            }
            d
        };
        let msg = match wait {
            Some(d) => match msg_rx.recv_timeout(d) {
                Ok(m) => Some(m),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            },
            None => match msg_rx.recv() {
                Ok(m) => Some(m),
                Err(_) => break,
            },
        };
        match msg {
            Some(Msg::Data(b)) => {
                // The interceptor splits the read into routed steps. Every step
                // except a rejection tees its bytes to the local terminal (which
                // renders sixel/kitty/iTerm2 natively); the match then handles the
                // mirror/app effect. `Step::tee` is the one place the tee lives.
                for step in interceptor.feed(&b) {
                    if let Some(bytes) = step.tee().filter(|_| connected) {
                        let _ = out.write_all(bytes); // immediate, not rate-limited
                        let _ = out.flush();
                    }
                    match step {
                        Step::Passthrough(x) => parser.process(&x), // → vt100 too
                        Step::TerminalOnly(_) => {}                 // already teed
                        Step::Image(_, img) => {
                            // Ready payload (iTerm2 native / kitty PNG): stamp
                            // and record in one step.
                            let hash = crate::proto::content_key(&img.mime, &img.bytes);
                            let ready = Some((
                                hash,
                                crate::model::ImageBlob {
                                    mime: img.mime,
                                    bytes: img.bytes.into(),
                                },
                            ));
                            stamp_image(
                                &mut parser,
                                &mut images,
                                &mut image_seq,
                                cell,
                                img.cells,
                                img.px,
                                ready,
                            );
                        }
                        Step::Deferred(_, d) => {
                            // Heavy decode (sixel / kitty raw): stamp NOW —
                            // cursor semantics can't wait — and let the worker
                            // owe the bytes. Newest-wins on the worker side:
                            // video frames superseded before decoding are
                            // never decoded (their pending placements die by
                            // cell overwrite like any other image).
                            let id = stamp_image(
                                &mut parser,
                                &mut images,
                                &mut image_seq,
                                cell,
                                d.cells,
                                Some(d.px),
                                None,
                            );
                            let _ = job_tx.send((id, d.payload));
                        }
                        Step::Reject(resp) => {
                            // Suppressed from the terminal (tee() was None), so it
                            // never services the refused transmission; answer the app
                            // ourselves to provoke a fallback to direct.
                            if let Ok(mut w) = inject.lock() {
                                let _ = w.write_all(&resp);
                                let _ = w.flush();
                            }
                        }
                    }
                }
                dirty = true;
            }
            Some(Msg::ImageReady { id, hash, blob }) => {
                // Fill the pending placement; a miss means it was already
                // evicted (overwritten/scrolled off) — drop the bytes.
                if let Some(p) = images.iter_mut().find(|p| p.id == id) {
                    p.ready = Some((hash, blob));
                    dirty = true;
                }
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
        // A bridged hidden cursor whose grace has now expired must publish the
        // real hide even with no new data (dirty=false).
        let hide_due = bridge.shown.is_some()
            && bridge
                .hidden_since
                .is_some_and(|t| t.elapsed() >= CURSOR_HIDE_GRACE);
        if (dirty || hide_due) && last_frame.elapsed() >= MIN_FRAME && !sync.hold(parser.screen()) {
            let _ = frame_tx.send(frame_from(&parser, &mut images, &mut zombies, &mut bridge));
            dirty = false;
            last_frame = Instant::now();
        }
    }
}

fn new_parser(rows: u16, cols: u16) -> SgParser {
    SgParser::new_with_callbacks(rows, cols, 0, SeqLog::new().0)
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

/// One short descriptor per CSI shape. Mode set/reset (`h`/`l`), SGR (`m`),
/// and XTWINOPS (`t`) gaps are per-parameter, so their params join the key;
/// for everything else the intermediates + final byte are the kind.
fn csi_kind(i1: Option<u8>, i2: Option<u8>, params: &[&[u16]], c: char) -> String {
    let mut s = String::from("CSI");
    for i in [i1, i2].into_iter().flatten() {
        s.push(' ');
        s.push(char::from(i));
    }
    // Param-carrying finals where the numbers ARE the diagnosis (modes, SGR,
    // window ops, DSR queries) — join them into the kind so an unknown one is
    // identifiable straight from the exit line.
    if matches!(c, 'h' | 'l' | 'm' | 't' | 'n') {
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

// Generic over the cell-data type: telemetry never touches cells.
impl<T> vt100::Callbacks<T> for SeqLog {
    fn unhandled_char(&mut self, _: &mut vt100::Screen<T>, c: char) {
        // U+FFFD is decode noise from binary output, not a sequence gap.
        if c != '\u{fffd}' {
            self.record(format!("CHAR U+{:04X}", u32::from(c)));
        }
    }
    fn unhandled_control(&mut self, _: &mut vt100::Screen<T>, b: u8) {
        self.record(format!("CTRL 0x{b:02x}"));
    }
    fn unhandled_escape(
        &mut self,
        _: &mut vt100::Screen<T>,
        i1: Option<u8>,
        i2: Option<u8>,
        b: u8,
    ) {
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
        _: &mut vt100::Screen<T>,
        i1: Option<u8>,
        i2: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        self.record(csi_kind(i1, i2, params, c));
    }
    fn unhandled_osc(&mut self, _: &mut vt100::Screen<T>, params: &[&[u8]]) {
        let selector = params
            .first()
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .unwrap_or_default();
        self.record(format!("OSC {selector}"));
    }
}

/// Frame gating for synchronized updates (DEC private mode 2026). While an
/// application is between BSU (`CSI ? 2026 h`) and ESU (`CSI ? 2026 l`) the
/// pending frame is held, so viewers never see a torn mid-redraw snapshot —
/// the same atomic presentation kitty gives the local screen (keeping the
/// previous frame up IS the spec's presentation semantics). neovim and modern
/// tmux wrap every redraw in these.
///
/// Two hazards are designed around:
/// - Every BSU restarts a hard deadline ([`SyncGate::MAX_HOLD`]): an app
///   killed between BSU and ESU must degrade to plain unsynchronized
///   mirroring, never freeze the mirror (the unterminated-image failure
///   class). Real terminals cap the same way.
/// - Animation loops batch `ESU BSU` into one PTY read, so the mode bit alone
///   reads "always in progress". The screen's BSU/ESU *counters* expose the
///   completed update: an ESU seen since the last decision publishes this
///   round (at read-boundary granularity) while the freshly-opened update
///   holds the next.
struct SyncGate {
    starts: u32,
    ends: u32,
    since: Instant,
}

impl SyncGate {
    /// Deadline for one synchronized update; beyond it the mirror publishes
    /// anyway.
    const MAX_HOLD: Duration = Duration::from_secs(1);

    fn new() -> SyncGate {
        SyncGate {
            starts: 0,
            ends: 0,
            since: Instant::now(),
        }
    }

    /// Should the pending publish be held? Call only when a publish is
    /// otherwise due — the call consumes the ESU-seen edge.
    fn hold<T>(&mut self, screen: &vt100::Screen<T>) -> bool {
        self.hold_within(screen, Self::MAX_HOLD)
    }

    fn hold_within<T>(&mut self, screen: &vt100::Screen<T>, max_hold: Duration) -> bool {
        let starts = screen.synchronized_update_starts();
        if starts != self.starts {
            self.starts = starts;
            self.since = Instant::now();
        }
        let presented = screen.synchronized_update_ends() != self.ends;
        self.ends = screen.synchronized_update_ends();
        screen.synchronized_update() && !presented && self.since.elapsed() < max_hold
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

/// An inline image plus the tag id its covered cells carry in the parser grid
/// (stored in the vendored vt100's per-cell data slot, `place_data`). The
/// tagged cells ride vt100's own
/// scrolling, eviction, and reflow, so the parser tracks the image's fate per
/// cell; `resolve_images` reads the survivors back each frame.
struct Placed {
    id: std::num::NonZeroU32,
    /// Resolved top-left, updated each frame from the surviving cell tags.
    row: i16,
    col: u16,
    /// Stamped display size in cells (fractional; see [`crate::model::ImagePlacement`]).
    cols: Option<f32>,
    rows: Option<f32>,
    /// Content address + payload. `None` while the decode worker still owes
    /// it: the placement is stamped (cursor semantics are exact from the
    /// moment the sequence arrives) but skipped in frames until the bytes
    /// exist — a frame must never reference an unservable hash. Rides every
    /// frame's `image_data` (by refcount) once ready, so each frame is
    /// self-contained for the standalone server and the push client's blob
    /// uploads. A placement superseded before its decode simply stays
    /// pending until vt100's cell lifetime evicts it — the "skip stale
    /// frames" half of the worker's newest-wins queue.
    ready: Option<(String, crate::model::ImageBlob)>,
}

/// Stamp an image at the cursor and start tracking it. Per-cell tags go into
/// the parser grid (`place_data`): vt100's own scroll/erase/reflow then manage
/// the image's lifetime cell by cell, exactly mirroring a cell-based sixel
/// terminal's erase semantics — text written over an image cell erases that
/// cell's share of the image; `resolve_images` reconstructs the top-left from
/// the surviving tags each frame and evicts once every covered cell is gone.
/// The cell size comes from the app's hint, else from pixel size ÷ cell size
/// (so a natural-size image still advances the cursor); placement leaves the
/// cursor at col 0 of the image's last row, where a sixel-scrolling terminal
/// leaves it. `ready` is the payload when already decoded, `None` while the
/// worker owes it.
fn stamp_image(
    parser: &mut SgParser,
    images: &mut Vec<Placed>,
    image_seq: &mut std::num::NonZeroU32,
    cell: (u16, u16),
    cells: Option<(u16, u16)>,
    px: Option<(u32, u32)>,
    ready: Option<(String, crate::model::ImageBlob)>,
) -> std::num::NonZeroU32 {
    let (row, col) = parser.screen().cursor_position();
    // Display size in cells. An app-specified footprint is exact; a derived one
    // is the TRUE fractional extent (pixels ÷ cell size, NOT rounded up) so the
    // viewer draws the image at the same grid fraction the terminal did — the
    // partial last row stays partly empty instead of a ceil'd full row that the
    // cursor (left on the image's last row) would then print text over.
    let disp = cells
        .map(|(c, r)| (f32::from(c), f32::from(r)))
        .or_else(|| {
            px.map(|(w, h)| {
                (
                    (w as f32 / f32::from(cell.0)).max(1.0),
                    (h as f32 / f32::from(cell.1)).max(1.0),
                )
            })
        });
    let (cols, rows) = disp.map_or((None, None), |(c, r)| (Some(c), Some(r)));
    // The parser tags whole cells (the image's grid lifetime), so it gets the
    // CEIL of the display extent: every cell the image touches is tracked and
    // managed by vt100's scroll/erase like a cell-based sixel terminal.
    let (fw, fh) = disp.map_or((1, 1), |(c, r)| {
        ((c.ceil() as u16).max(1), (r.ceil() as u16).max(1))
    });
    let id = *image_seq;
    *image_seq = image_seq
        .checked_add(1)
        .unwrap_or(std::num::NonZeroU32::MIN);
    parser
        .screen_mut()
        .place_data(fw, fh, |row_off, col_off| ImgCell {
            id,
            row_off,
            col_off,
        });
    images.push(Placed {
        id,
        row: i16::try_from(row).unwrap_or(0),
        col,
        cols,
        rows,
        ready,
    });
    id
}

/// Snapshot the PTY screen as a [`Frame`], resolving each tracked image's tagged
/// cells to its current position and dropping images with no covered cell left
/// (scrolled off the top, cleared, or overwritten).
/// Cursor-hidden debounce state (see [`CURSOR_HIDE_GRACE`]). Tracks the last
/// position seen while the cursor was shown and when the app most recently hid
/// it, so a brief redraw-transient hide keeps showing the last cursor instead of
/// blinking the mirror.
#[derive(Default)]
struct CursorBridge {
    shown: Option<(u16, u16)>,     // last position while the cursor was visible
    style: u8,                     // its DECSCUSR style, to restore alongside
    hidden_since: Option<Instant>, // when the app last hid the cursor (None ⇒ shown)
}

impl CursorBridge {
    /// Apply the debounce to a freshly built grid: while the cursor is shown,
    /// remember it; while it's hidden but within the grace window, override the
    /// grid back to the last-shown cursor. Returns the instant the hide began
    /// while still bridging (so the caller can schedule a wake to publish the
    /// real hide at grace expiry), else `None`.
    fn apply(&mut self, grid: &mut crate::model::Grid, now: Instant) -> Option<Instant> {
        match grid.cursor {
            Some(pos) => {
                self.shown = Some(pos);
                self.style = grid.cursor_style;
                self.hidden_since = None;
                None
            }
            None => {
                let since = *self.hidden_since.get_or_insert(now);
                if now.saturating_duration_since(since) < CURSOR_HIDE_GRACE {
                    grid.cursor = self.shown; // keep the last-shown cursor visible
                    grid.cursor_style = self.style;
                    self.shown.map(|_| since) // still bridging (only if we had a cursor to hold)
                } else {
                    self.shown = None; // grace elapsed: the hide is genuine
                    None
                }
            }
        }
    }
}

fn frame_from(
    parser: &SgParser,
    images: &mut Vec<Placed>,
    zombies: &mut Vec<Placed>,
    bridge: &mut CursorBridge,
) -> Arc<Frame> {
    let mut grid = crate::parse::grid_from_screen(parser.screen());
    bridge.apply(&mut grid, Instant::now());
    grid.images = resolve_images(parser.screen(), images, zombies);
    grid.image_data = images
        .iter()
        .chain(zombies.iter())
        .filter_map(|p| {
            let (hash, blob) = p.ready.as_ref()?;
            Some((hash.clone(), blob.clone()))
        })
        .collect();
    Arc::new(Frame::Screen(grid))
}

/// The stamped cell rectangles of two placements overlap.
fn rects_overlap(a: &Placed, b: &Placed) -> bool {
    let (ar, ac) = (i32::from(a.row), i32::from(a.col));
    let (br, bc) = (i32::from(b.row), i32::from(b.col));
    // Ceil to whole cells: overlap is a grid-cell question (which cells a
    // pending successor shares with a zombie), the fractional tail rounds up.
    let (aw, ah) = (
        a.cols.map_or(1, |c| c.ceil() as i32),
        a.rows.map_or(1, |r| r.ceil() as i32),
    );
    let (bw, bh) = (
        b.cols.map_or(1, |c| c.ceil() as i32),
        b.rows.map_or(1, |r| r.ceil() as i32),
    );
    ar < br + bh && br < ar + ah && ac < bc + bw && bc < ac + aw
}

/// For each tracked image, find its surviving tagged cells in the parser grid and
/// reconstruct the top-left from the most top-left survivor — each tag stores its
/// in-image offset, so any survivor reconstructs it exactly (a bottom-row survivor
/// yields a negative row once the image has partially scrolled off the top, and
/// the viewer clips it). Drop images with no surviving cell — fully scrolled off,
/// cleared, or wholly overwritten, which is exactly when the terminal no longer
/// shows any part of them either.
///
/// EXCEPT the double-buffer case: a READY image wholly overwritten by a still-
/// PENDING one (sixel video: frame N+1's stamp evicts frame N before its bytes
/// arrive) becomes a ZOMBIE — emitted with frozen coordinates until a pending
/// placement no longer overlaps its rect (the successor's bytes landed, or it
/// died too). Without it, every video frame flashes an image-less full at
/// viewers. A clear/overwrite with no pending successor drops the image
/// instantly, so nothing lingers that the terminal doesn't also show.
fn resolve_images(
    screen: &vt100::Screen<ImgCell>,
    images: &mut Vec<Placed>,
    zombies: &mut Vec<Placed>,
) -> Vec<ImagePlacement> {
    if !images.is_empty() {
        // One pass over the grid (not one per image, and skipped entirely in the
        // imageless common case): the minimal-offset survivor per tracked image,
        // as (row_off, col_off, screen_row, screen_col).
        let mut best: Vec<Option<(u16, u16, u16, u16)>> = vec![None; images.len()];
        let (srows, scols) = screen.size();
        for r in 0..srows {
            for c in 0..scols {
                let Some(&icell) = screen.cell(r, c).and_then(vt100::Cell::data) else {
                    continue;
                };
                let Some(i) = images.iter().position(|p| p.id == icell.id) else {
                    continue;
                };
                let off = (icell.row_off, icell.col_off);
                if best[i].is_none_or(|(dr, dc, _, _)| off < (dr, dc)) {
                    best[i] = Some((off.0, off.1, r, c));
                }
            }
        }
        let mut best = best.into_iter();
        let mut evicted = Vec::new();
        images.retain_mut(|p| {
            let Some(Some((dr, dc, r, c))) = best.next() else {
                evicted.push(std::mem::replace(
                    p,
                    Placed {
                        id: p.id,
                        row: 0,
                        col: 0,
                        cols: None,
                        rows: None,
                        ready: None,
                    },
                ));
                return false; // every covered cell gone → evict
            };
            p.row = i16::try_from(r).unwrap_or(i16::MAX) - i16::try_from(dr).unwrap_or(i16::MAX);
            p.col = c.saturating_sub(dc);
            true
        });
        // Evicted-but-ready placements overlapped by a pending successor keep
        // showing (frozen where they last resolved) until the swap completes.
        zombies.extend(evicted.into_iter().filter(|z| {
            z.ready.is_some()
                && images
                    .iter()
                    .any(|p| p.ready.is_none() && rects_overlap(z, p))
        }));
    }
    // A zombie lives exactly as long as some pending placement overlaps it.
    zombies.retain(|z| {
        images
            .iter()
            .any(|p| p.ready.is_none() && rects_overlap(z, p))
    });
    // Emit zombies FIRST: a live placement covering the same cells must paint
    // over its predecessor, and the viewer draws the list in order. Pending
    // placements (decode worker still owes the bytes) stay tracked — their
    // cells live in the grid — but are not emitted: a frame must never
    // reference a hash no server can satisfy.
    zombies
        .iter()
        .chain(images.iter())
        .filter_map(|p| {
            let (hash, _) = p.ready.as_ref()?;
            Some(ImagePlacement {
                row: p.row,
                col: p.col,
                cols: p.cols,
                rows: p.rows,
                hash: hash.clone(),
            })
        })
        .collect()
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

/// EXPERIMENTAL: rewrites the terminal→child input stream so a Primary DA reply
/// (`ESC [ ? <params> c`) advertises sixel (feature `4`) even though the real
/// terminal doesn't — the child (and tmux) then emit sixel, which we transcode.
/// Conservative: only a fully-formed `ESC [ ? <digits/;> c` is touched; anything
/// else (keystrokes, other reports) passes verbatim. A DA reply split across reads
/// is carried; the carry is bounded so a stray `ESC [ ?` can't wedge input.
#[derive(Default)]
struct DaRewriter {
    carry: Vec<u8>,
}

impl DaRewriter {
    /// Longest DA reply we'll wait to complete before giving up and flushing.
    const MAX_CARRY: usize = 64;

    fn advertise_sixel(&mut self, input: &[u8]) -> Vec<u8> {
        let mut data = std::mem::take(&mut self.carry);
        data.extend_from_slice(input);
        let mut out = Vec::with_capacity(data.len() + 2);
        let mut i = 0;
        while i < data.len() {
            // Only `ESC [ ?` can begin a DA reply; everything else is verbatim.
            if data[i] != 0x1b {
                out.push(data[i]);
                i += 1;
                continue;
            }
            let rest = &data[i..];
            if !rest.starts_with(b"\x1b[?") {
                // Not a DA start. If it's a truncated prefix of one at the buffer
                // end, carry it; else emit the ESC and rescan.
                if b"\x1b[?".starts_with(rest) {
                    self.carry = rest.to_vec();
                    return out;
                }
                out.push(data[i]);
                i += 1;
                continue;
            }
            // Scan params (digits and ';') to the terminator.
            let mut j = i + 3;
            while j < data.len() && (data[j].is_ascii_digit() || data[j] == b';') {
                j += 1;
            }
            match data.get(j) {
                Some(b'c') => {
                    let params = &data[i + 3..j];
                    out.extend_from_slice(&data[i..j]); // ESC [ ? params
                    if !params.split(|&b| b == b';').any(|f| f == b"4") {
                        out.extend_from_slice(b";4");
                    }
                    out.push(b'c');
                    i = j + 1;
                }
                // A `?`-CSI that isn't DA (e.g. DECRPM `…$y`): emit the params and
                // rescan from the terminator byte (it may be an ESC starting the
                // next sequence), so we never swallow a following DA reply.
                Some(_) => {
                    out.extend_from_slice(&data[i..j]);
                    i = j;
                }
                // Terminator not here yet — carry, unless it's grown implausibly.
                None => {
                    if data.len() - i <= Self::MAX_CARRY {
                        self.carry = data[i..].to_vec();
                    } else {
                        out.extend_from_slice(&data[i..]);
                    }
                    return out;
                }
            }
        }
        out
    }
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

    fn grid_with_cursor(cursor: Option<(u16, u16)>) -> crate::model::Grid {
        crate::model::Grid {
            cols: 80,
            rows: Vec::new(),
            cursor,
            cursor_style: 1,
            default_colors: (crate::model::Color::Default, crate::model::Color::Default),
            title: String::new(),
            links: Default::default(),
            images: Vec::new(),
            image_data: Default::default(),
        }
    }

    #[test]
    fn cursor_bridge_debounces_transient_hide() {
        let mut b = CursorBridge::default();
        let t = Instant::now();

        // Cursor shown → remembered, no bridging.
        let mut g = grid_with_cursor(Some((63, 2)));
        assert_eq!(b.apply(&mut g, t), None);
        assert_eq!(g.cursor, Some((63, 2)));

        // App hides it (redraw start); within grace we keep showing the last pos.
        let mut g = grid_with_cursor(None);
        assert!(b.apply(&mut g, t + Duration::from_millis(20)).is_some());
        assert_eq!(
            g.cursor,
            Some((63, 2)),
            "transient hide bridged to last cursor"
        );

        // App re-shows it (redraw end) before grace expires: no hide ever shipped.
        let mut g = grid_with_cursor(Some((63, 2)));
        assert_eq!(b.apply(&mut g, t + Duration::from_millis(40)), None);
        assert_eq!(g.cursor, Some((63, 2)));
    }

    #[test]
    fn cursor_bridge_honors_a_genuine_hide() {
        let mut b = CursorBridge::default();
        let t = Instant::now();
        b.apply(&mut grid_with_cursor(Some((5, 5))), t); // establish a shown cursor

        // Hidden and stays hidden past the grace window → the real hide ships.
        let hidden_at = t + Duration::from_millis(10);
        let mut g = grid_with_cursor(None);
        b.apply(&mut g, hidden_at); // bridged; the hide clock starts here
        assert_eq!(g.cursor, Some((5, 5)));
        let mut g = grid_with_cursor(None);
        assert_eq!(
            b.apply(
                &mut g,
                hidden_at + CURSOR_HIDE_GRACE + Duration::from_millis(1)
            ),
            None
        );
        assert_eq!(g.cursor, None, "genuine hide honored after grace");
    }

    #[test]
    fn cursor_bridge_does_not_invent_a_cursor() {
        // Hidden from the start (never shown) → nothing to bridge to.
        let mut b = CursorBridge::default();
        let mut g = grid_with_cursor(None);
        assert_eq!(b.apply(&mut g, Instant::now()), None);
        assert_eq!(g.cursor, None);
    }

    #[test]
    fn da_rewriter_advertises_sixel() {
        let mut da = DaRewriter::default();
        // Adds feature 4 to a DA reply that lacks it, verbatim otherwise.
        assert_eq!(da.advertise_sixel(b"\x1b[?62;22c"), b"\x1b[?62;22;4c");
        // Already advertises sixel → untouched.
        assert_eq!(da.advertise_sixel(b"\x1b[?62;4;22c"), b"\x1b[?62;4;22c");
        // Non-DA input (keystrokes, a cursor-position report) passes verbatim.
        assert_eq!(da.advertise_sixel(b"ls -la\r"), b"ls -la\r");
        assert_eq!(da.advertise_sixel(b"\x1b[10;5R"), b"\x1b[10;5R");
        // A DECRPM `?`-CSI (ends in $y, not c) is left alone.
        assert_eq!(da.advertise_sixel(b"\x1b[?2026;2$y"), b"\x1b[?2026;2$y");
    }

    #[test]
    fn da_rewriter_reassembles_a_split_reply() {
        let mut da = DaRewriter::default();
        // DA reply split across two reads — carried, then completed and rewritten.
        assert_eq!(da.advertise_sixel(b"\x1b[?62;"), b"");
        assert_eq!(da.advertise_sixel(b"22c rest"), b"\x1b[?62;22;4c rest");
    }

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
        let mut parser = SgParser::new_with_callbacks(24, 80, 0, seqlog);
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

    // The five kinds from the first real exit report (roadmap phase 1.6):
    // queries and string syntax are deliberate parser no-ops and must stay out
    // of the report — as is the OSC 10/11 *set* form now that item 9 mirrors
    // it. An XTWINOPS op outside the known-harmless set still reports, with
    // its params in the kind so the exit line names the op; so does an
    // unparseable OSC color value.
    #[test]
    fn seqlog_silent_on_query_noise_loud_on_real_gaps() {
        let (seqlog, seen) = SeqLog::new();
        let mut parser = SgParser::new_with_callbacks(24, 80, 0, seqlog);
        parser.process(b"\x1b[c\x1b[>c\x1b[14t\x1b[18t\x1b[22;0t\x1b[23;0t");
        parser.process(b"\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b]11;#300a24\x1b\\");
        assert!(
            seen.lock().unwrap().is_empty(),
            "query noise and the mirrored set form must be silent"
        );
        parser.process(b"\x1b]11;papayawhip\x1b\\\x1b[9;1t");
        assert_eq!(
            seen.lock().unwrap().iter().cloned().collect::<Vec<_>>(),
            vec!["CSI 9 1 t".to_string(), "OSC 11".to_string()]
        );
    }

    // The five kinds from the second exit report (roadmap phase 1.7): DECAWM
    // and IRM are modeled now, cursor blink and SCS-ASCII are deliberate
    // no-ops — all silent. ESC ( 0 (DEC line drawing) is a real gap and must
    // stay loud.
    #[test]
    fn seqlog_silent_on_round3_kinds() {
        let (seqlog, seen) = SeqLog::new();
        let mut parser = SgParser::new_with_callbacks(24, 80, 0, seqlog);
        parser.process(b"\x1b[4h\x1b[4l\x1b[?7h\x1b[?7l\x1b[?12h\x1b[?12l");
        parser.process(b"\x1b(B\x1b)B");
        assert!(seen.lock().unwrap().is_empty(), "round-3 kinds are handled");
        parser.process(b"\x1b(0");
        assert_eq!(
            seen.lock().unwrap().iter().cloned().collect::<Vec<_>>(),
            vec!["ESC ( 0".to_string()]
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

    // Synchronized-update gating: hold between BSU and ESU, publish on ESU
    // even when the next BSU arrives in the same read, and never hold past
    // the deadline.
    #[test]
    fn sync_gate_holds_between_bsu_and_esu() {
        let mut parser = new_parser(24, 80);
        let mut gate = SyncGate::new();
        assert!(!gate.hold(parser.screen()), "no sync in sight: publish");
        parser.process(b"\x1b[?2026hpartial redraw");
        assert!(gate.hold(parser.screen()), "mid-update: hold");
        parser.process(b"rest of redraw\x1b[?2026l");
        assert!(!gate.hold(parser.screen()), "update completed: publish");
        assert!(!gate.hold(parser.screen()), "still idle: publish");

        // An animation loop batches `ESU BSU` into one read: the completed
        // update publishes this round, the freshly-opened one holds the next.
        parser.process(b"\x1b[?2026hframe\x1b[?2026l\x1b[?2026hpartial");
        assert!(
            !gate.hold(parser.screen()),
            "a full update completed: publish"
        );
        assert!(gate.hold(parser.screen()), "the new update holds again");
    }

    #[test]
    fn sync_gate_deadline_never_freezes_the_mirror() {
        let mut parser = new_parser(24, 80);
        let mut gate = SyncGate::new();
        // An app killed between BSU and ESU: past the deadline the gate opens.
        parser.process(b"\x1b[?2026hstuck");
        assert!(!gate.hold_within(parser.screen(), Duration::ZERO));
        // A later, well-behaved update still gets a fresh hold budget: the
        // batched ESU publishes once, then the new update holds.
        parser.process(b"\x1b[?2026l\x1b[?2026hnext");
        assert!(
            !gate.hold(parser.screen()),
            "the stuck update's ESU publishes"
        );
        assert!(gate.hold(parser.screen()), "the fresh update holds");
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

    // A derived footprint (no app-specified cells) carries the TRUE fractional
    // extent for display, while the parser tags the CEIL in whole cells. This
    // is the image/prompt-overlap fix: a 400×402 image on 9×21 cells is 44.4 ×
    // 19.14 cells; the viewer draws it 19.14 rows tall so the partial last row
    // stays empty for the prompt, instead of a ceil'd 20th row it would sit on.
    #[test]
    fn derived_footprint_is_fractional_but_tags_whole_cells() {
        let mut parser = new_parser(24, 80);
        let mut images = Vec::new();
        let mut seq = std::num::NonZeroU32::MIN;
        stamp_image(
            &mut parser,
            &mut images,
            &mut seq,
            (9, 21),          // cell px
            None,             // no app-specified cells → derive from px
            Some((400, 402)), // image px
            None,
        );
        let p = &images[0];
        assert!(
            (p.cols.unwrap() - 400.0 / 9.0).abs() < 1e-3,
            "fractional cols"
        );
        assert!(
            (p.rows.unwrap() - 402.0 / 21.0).abs() < 1e-3,
            "fractional rows"
        );
        // The grid tags the ceil (45 × 20): the last col/row are covered.
        let s = parser.screen();
        assert!(
            s.cell(0, 44).and_then(vt100::Cell::data).is_some(),
            "45th col tagged"
        );
        assert!(
            s.cell(19, 0).and_then(vt100::Cell::data).is_some(),
            "20th row tagged"
        );
        assert!(
            s.cell(20, 0).and_then(vt100::Cell::data).is_none(),
            "21st row is free"
        );
        // Cursor is left on the image's last (20th) row, where text will print —
        // the row the fractional display leaves mostly empty on the viewer.
        assert_eq!(parser.screen().cursor_position().0, 19);
    }

    /// Place a `w`×`h` image at the parser's cursor, exactly as the screen
    /// thread does: stamp the cell tags and record the placement.
    fn place(parser: &mut SgParser, w: u16, h: u16) -> Vec<Placed> {
        let (row, col) = parser.screen().cursor_position();
        let id = std::num::NonZeroU32::MIN;
        parser
            .screen_mut()
            .place_data(w, h, |row_off, col_off| ImgCell {
                id,
                row_off,
                col_off,
            });
        vec![Placed {
            id,
            row: i16::try_from(row).unwrap_or(0),
            col,
            cols: Some(f32::from(w)),
            rows: Some(f32::from(h)),
            ready: Some((
                "ab".repeat(32),
                crate::model::ImageBlob {
                    mime: "image/png".into(),
                    bytes: bytes::Bytes::new(),
                },
            )),
        }]
    }

    // A deferred placement (worker still owes the bytes) is stamped — its
    // cells are tracked, cursor semantics exact — but INVISIBLE in frames:
    // no placement, no image_data, because a frame must never reference a
    // hash no server can satisfy. It appears the moment the payload lands.
    #[test]
    fn pending_image_invisible_until_ready() {
        let mut parser = new_parser(24, 80);
        let mut images = place(&mut parser, 4, 2);
        images[0].ready = None; // decode worker still owes the payload
        let Frame::Screen(g) = &*frame_from(
            &parser,
            &mut images,
            &mut Vec::new(),
            &mut CursorBridge::default(),
        );
        assert!(g.images.is_empty(), "pending placement not emitted");
        assert!(g.image_data.is_empty(), "no payload to carry");
        assert_eq!(images.len(), 1, "but still tracked (cells live)");

        // Msg::ImageReady's effect: the fill makes it visible.
        images[0].ready = Some((
            "cd".repeat(32),
            crate::model::ImageBlob {
                mime: "image/png".into(),
                bytes: bytes::Bytes::from_static(b"png-ish"),
            },
        ));
        let Frame::Screen(g) = &*frame_from(
            &parser,
            &mut images,
            &mut Vec::new(),
            &mut CursorBridge::default(),
        );
        assert_eq!(g.images.len(), 1);
        assert_eq!(g.images[0].hash, "cd".repeat(32));
        assert!(g.image_data.contains_key(&"cd".repeat(32)));
    }

    // The double-buffer swap (sixel video): a READY image overwritten by a
    // PENDING successor keeps showing — frozen — until the successor's bytes
    // land, then swaps atomically. Viewers never see an image-less gap
    // between video frames. An overwrite with no pending successor (a clear)
    // still vanishes immediately.
    #[test]
    fn ready_image_zombies_until_pending_successor_lands() {
        let mut parser = new_parser(24, 80);
        let mut zombies: Vec<Placed> = Vec::new();
        // Frame N: ready, on screen.
        let mut images = place(&mut parser, 4, 2);
        images[0].ready = Some((
            "aa".repeat(32),
            crate::model::ImageBlob {
                mime: "image/png".into(),
                bytes: bytes::Bytes::from_static(b"N"),
            },
        ));
        // Frame N+1: stamped over the same cells, decode pending. `place`
        // reuses id MIN, so give the successor its own id.
        parser.process(b"\x1b[H");
        let id2 = std::num::NonZeroU32::new(2).unwrap();
        parser
            .screen_mut()
            .place_data(4, 2, |row_off, col_off| ImgCell {
                id: id2,
                row_off,
                col_off,
            });
        images.push(Placed {
            id: id2,
            row: 0,
            col: 0,
            cols: Some(4.0),
            rows: Some(2.0),
            ready: None,
        });

        // N's cells are gone, but N keeps showing (zombie) while N+1 pends.
        let Frame::Screen(g) = &*frame_from(
            &parser,
            &mut images,
            &mut zombies,
            &mut CursorBridge::default(),
        );
        assert_eq!(g.images.len(), 1, "the old frame bridges the gap");
        assert_eq!(g.images[0].hash, "aa".repeat(32));
        assert!(
            g.image_data.contains_key(&"aa".repeat(32)),
            "zombie payload still rides the frame"
        );

        // N+1's bytes land → atomic swap: N gone, N+1 shown.
        images.iter_mut().find(|p| p.id == id2).unwrap().ready = Some((
            "bb".repeat(32),
            crate::model::ImageBlob {
                mime: "image/png".into(),
                bytes: bytes::Bytes::from_static(b"N+1"),
            },
        ));
        let Frame::Screen(g) = &*frame_from(
            &parser,
            &mut images,
            &mut zombies,
            &mut CursorBridge::default(),
        );
        assert_eq!(g.images.len(), 1, "swap complete");
        assert_eq!(g.images[0].hash, "bb".repeat(32));
        assert!(zombies.is_empty(), "zombie released");

        // A plain overwrite (no pending successor) vanishes immediately.
        parser.process(b"\x1b[2J");
        let Frame::Screen(g) = &*frame_from(
            &parser,
            &mut images,
            &mut zombies,
            &mut CursorBridge::default(),
        );
        assert!(g.images.is_empty(), "cleared image gone, no zombie");
        assert!(zombies.is_empty());
    }

    /// The tagged cells ride vt100's own scrolling: the reported top row falls
    /// as the screen scrolls, goes negative while the image clips against the
    /// top edge, and the image is only evicted once even its bottom row is gone.
    #[test]
    fn image_tracks_scroll_clips_then_evicts() {
        let mut parser = new_parser(3, 10); // 3 rows
        parser.process(b"\r\n\r\n"); // cursor to the last row
        // Placing a 2-row image at the bottom scrolls once mid-placement.
        let mut imgs = place(&mut parser, 2, 2);
        let Frame::Screen(g) = &*frame_from(
            &parser,
            &mut imgs,
            &mut Vec::new(),
            &mut CursorBridge::default(),
        );
        assert_eq!((g.images[0].row, g.images[0].col), (1, 0));

        // One scroll lifts the image → top row 0.
        parser.process(b"\r\nx");
        let Frame::Screen(g) = &*frame_from(
            &parser,
            &mut imgs,
            &mut Vec::new(),
            &mut CursorBridge::default(),
        );
        assert_eq!(g.images[0].row, 0);

        // Another: top row scrolls off → top row -1, image still shown (clipped).
        parser.process(b"\r\ny");
        let Frame::Screen(g) = &*frame_from(
            &parser,
            &mut imgs,
            &mut Vec::new(),
            &mut CursorBridge::default(),
        );
        assert_eq!(g.images[0].row, -1);

        // One more: the bottom row is gone too → the image is evicted.
        parser.process(b"\r\nz");
        let Frame::Screen(g) = &*frame_from(
            &parser,
            &mut imgs,
            &mut Vec::new(),
            &mut CursorBridge::default(),
        );
        assert!(g.images.is_empty());
    }

    // A surviving top row keeps the image alive when its bottom row is
    // overwritten in place — e.g. a shell prompt repainted right after a raw
    // `cat image.sixel`, which adds no trailing newline. The image reports its
    // true top row and is not evicted.
    #[test]
    fn top_row_survives_bottom_overwrite() {
        let mut parser = new_parser(4, 10);
        // A 2×2 image at (0,0); placement leaves the cursor at col 0 of row 1.
        let mut imgs = place(&mut parser, 2, 2);

        // A prompt repaints the bottom row, clobbering both of its image cells.
        parser.process(b"user@host$");
        let Frame::Screen(g) = &*frame_from(
            &parser,
            &mut imgs,
            &mut Vec::new(),
            &mut CursorBridge::default(),
        );
        // Still tracked, via the top row's cells, at its true top row.
        assert_eq!(g.images.len(), 1);
        assert_eq!((g.images[0].row, g.images[0].col), (0, 0));
    }

    // A 1-row image wider than the prompt survives via the cells right of it:
    // the prompt erases the left cells (and, in the terminal, that part of the
    // image), any survivor reconstructs the original top-left from its offset.
    #[test]
    fn one_row_image_survives_prompt_via_surviving_cells() {
        let mut parser = new_parser(4, 30);
        // a 20-cell-wide, 1-row image at col 0.
        let mut imgs = place(&mut parser, 20, 1);

        // The prompt covers cols 0..11 — cells 11..20 survive.
        parser.process(b"user@host$ ");
        let Frame::Screen(g) = &*frame_from(
            &parser,
            &mut imgs,
            &mut Vec::new(),
            &mut CursorBridge::default(),
        );
        assert_eq!(g.images.len(), 1);
        assert_eq!((g.images[0].row, g.images[0].col), (0, 0));
    }

    // Corner-sentinel tracking couldn't do this: overwrite *both* ends of a
    // 1-row image and it lived on only via interior cells. Per-cell tags keep
    // it alive as long as any covered cell survives — like the terminal, which
    // still shows the image's middle.
    #[test]
    fn interior_cells_keep_image_alive() {
        let mut parser = new_parser(4, 10);
        let mut imgs = place(&mut parser, 3, 1);
        // Overwrite the leftmost and rightmost image cells; the middle survives.
        parser.process(b"x\x1b[3Gy");
        let Frame::Screen(g) = &*frame_from(
            &parser,
            &mut imgs,
            &mut Vec::new(),
            &mut CursorBridge::default(),
        );
        assert_eq!(g.images.len(), 1);
        assert_eq!((g.images[0].row, g.images[0].col), (0, 0));
    }

    // Wide glyphs occupy two screen columns — a CJK/emoji prompt before the
    // image must not drag the reconstructed column left, or the browser draws
    // the overlay shifted onto the preceding text.
    #[test]
    fn image_column_correct_after_wide_glyphs() {
        let mut parser = new_parser(4, 30);
        parser.process("日本語 ".as_bytes()); // three wide glyphs + a space → col 7
        let mut imgs = place(&mut parser, 2, 2);
        let Frame::Screen(g) = &*frame_from(
            &parser,
            &mut imgs,
            &mut Vec::new(),
            &mut CursorBridge::default(),
        );
        assert_eq!((g.images[0].row, g.images[0].col), (0, 7));
    }

    // A 1-row image narrower than what overwrites it is evicted — every covered
    // cell is erased, which is exactly when the terminal has erased the whole
    // image too.
    #[test]
    fn fully_overwritten_image_evicts() {
        let mut parser = new_parser(4, 30);
        // a 5-cell-wide, 1-row image at col 0.
        let mut imgs = place(&mut parser, 5, 1);

        // An 11-char prompt paints across the entire image row.
        parser.process(b"user@host$ ");
        let Frame::Screen(g) = &*frame_from(
            &parser,
            &mut imgs,
            &mut Vec::new(),
            &mut CursorBridge::default(),
        );
        assert!(g.images.is_empty());
        assert!(imgs.is_empty());
    }
}

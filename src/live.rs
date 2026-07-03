//! Live tracking input source: a persistent `tmux -C` (control mode) client.
//!
//! It attaches once, seeds each pane's [`vt100::Parser`] from a single capture,
//! then feeds the incremental `%output` byte stream tmux pushes as panes produce
//! it — no polling. A dedicated OS thread owns the parsers and, when the stream
//! goes quiet (or at most ~30fps), renders the fragment and publishes it on a
//! `watch` channel that the SSE endpoint relays to browsers.
//!
//! Scope: tracks the target session's *current window*. Window switching / layout
//! changes trigger a full re-capture, which also fixes geometry, so we never parse
//! tmux's layout string ourselves.

use crate::config::Config;
use crate::fonts::Resolver;
use crate::model::{Pane, PaneGeom, Window};
use crate::{parse, render, tmux};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use tokio::sync::watch;

/// One live pane: its placement plus a long-lived parser fed by `%output`.
struct LivePane {
    geom: PaneGeom,
    parser: vt100::Parser,
}

/// Whole tracked window: size + panes, rebuilt on every layout change.
struct State {
    width: u16,
    height: u16,
    panes: Vec<LivePane>,
}

/// Coalesce bursts and cap the publish rate here (a fast-scrolling pane can emit
/// thousands of `%output` lines/sec; the browser can't use more than a few dozen
/// frames/sec anyway). ponytail: fixed 30fps ceiling, expose as a flag if needed.
const MIN_FRAME: Duration = Duration::from_millis(33);

/// Start live tracking. Returns a `watch::Receiver` whose value is always the
/// latest rendered `#screen` fragment (or an error banner). Never fails: tmux
/// problems surface as an in-page banner rather than a failed request.
pub fn start(
    target: Option<String>,
    config: Arc<Config>,
    resolver: Arc<Resolver>,
) -> watch::Receiver<String> {
    // Seed synchronously so `GET /` has content on the very first request.
    let initial = match resync(target.as_deref()) {
        Ok(state) => render_state(&state, &config, &resolver),
        Err(e) => banner(&e.to_string()),
    };
    let (tx, rx) = watch::channel(initial);

    std::thread::spawn(move || run(target, config, resolver, tx));
    rx
}

fn run(
    target: Option<String>,
    config: Arc<Config>,
    resolver: Arc<Resolver>,
    tx: watch::Sender<String>,
) {
    let mut state = match resync(target.as_deref()) {
        Ok(s) => s,
        Err(e) => {
            let _ = tx.send(banner(&e.to_string()));
            // Fall through to control mode anyway: tmux may come up, or it's
            // already up and only the current window momentarily had no panes.
            State {
                width: 0,
                height: 0,
                panes: Vec::new(),
            }
        }
    };

    let mut child = match spawn_control(target.as_deref()) {
        Ok(c) => c,
        Err(e) => {
            let _ = tx.send(banner(&format!("control mode: {e}")));
            return;
        }
    };

    // Match our control client's size to the window so attaching doesn't resize
    // the user's real session. ponytail: assumes window-size=latest/smallest is
    // sane; if a live client still forces a resize, set `window-size manual`.
    if let (Some(stdin), true) = (child.stdin.as_mut(), state.width > 0) {
        let _ = writeln!(stdin, "refresh-client -C {}x{}", state.width, state.height);
    }

    let Some(stdout) = child.stdout.take() else {
        let _ = tx.send(banner("control mode produced no stdout"));
        return;
    };

    // Reader thread: drain the control stream into a channel as fast as it arrives,
    // so parsing/rendering never stalls the read (a high-throughput pane can emit
    // thousands of %output lines/sec). Rendering happens on this thread, strictly
    // rate-limited below — otherwise a flood renders thousands of frames/sec and
    // drowns the browser in innerHTML swaps.
    let (line_tx, line_rx) = mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = Vec::new();
        loop {
            line.clear();
            match reader.read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => break, // tmux exited / control mode ended
                Ok(_) => {}
            }
            strip_eol(&mut line);
            if line_tx.send(std::mem::take(&mut line)).is_err() {
                break;
            }
        }
    });

    let _ = tx.send(render_state(&state, &config, &resolver));
    let mut last_frame = Instant::now();
    let mut dirty = false;

    'outer: loop {
        // Block for the next line when idle; when a frame is pending, wait at most
        // until it's due so an idle update still flushes within MIN_FRAME.
        let next = if dirty {
            match line_rx.recv_timeout(MIN_FRAME) {
                Ok(l) => Some(l),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        } else {
            match line_rx.recv() {
                Ok(l) => Some(l),
                Err(_) => break,
            }
        };

        if let Some(line) = next {
            match handle_line(&line, &mut state, target.as_deref()) {
                Step::Dirty => dirty = true,
                Step::Idle => {}
                Step::Exit => break,
            }
            // Coalesce the burst: drain queued lines until a frame is due, so we
            // render the whole burst once rather than per line.
            while last_frame.elapsed() < MIN_FRAME {
                match line_rx.try_recv() {
                    Ok(l) => match handle_line(&l, &mut state, target.as_deref()) {
                        Step::Dirty => dirty = true,
                        Step::Idle => {}
                        Step::Exit => break 'outer,
                    },
                    Err(_) => break,
                }
            }
        }

        // Hard 30fps cap — the whole point of this rewrite.
        if dirty && last_frame.elapsed() >= MIN_FRAME {
            let _ = tx.send(render_state(&state, &config, &resolver));
            dirty = false;
            last_frame = Instant::now();
        }
    }

    let _ = child.wait();
    let _ = tx.send(banner("tmux control mode ended"));
}

/// Effect of one control-mode line on the tracked state.
enum Step {
    Dirty,
    Idle,
    Exit,
}

fn handle_line(line: &[u8], state: &mut State, target: Option<&str>) -> Step {
    match classify(line) {
        Event::Output { pane, data } => {
            match state
                .panes
                .iter_mut()
                .find(|p| p.geom.id.as_bytes() == pane)
            {
                Some(p) => {
                    p.parser.process(&unescape(data));
                    Step::Dirty
                }
                None => Step::Idle,
            }
        }
        // Any structural change: re-capture. Cheap (only on real changes) and it
        // re-seeds contents at the new geometry, so resize is handled too.
        Event::Resync => match resync(target) {
            Ok(s) => {
                *state = s;
                Step::Dirty
            }
            Err(_) => Step::Idle,
        },
        Event::Exit => Step::Exit,
        Event::Ignore => Step::Idle,
    }
}

/// Capture the target's current window and build fresh per-pane parsers.
fn resync(target: Option<&str>) -> anyhow::Result<State> {
    let raw = tmux::capture(target)?;
    let panes = raw
        .panes
        .into_iter()
        .map(|p| LivePane {
            parser: parse::seed_parser(&p.capture, p.geom.width, p.geom.height, p.cursor),
            geom: p.geom,
        })
        .collect();
    Ok(State {
        width: raw.width,
        height: raw.height,
        panes,
    })
}

fn render_state(state: &State, config: &Config, resolver: &Resolver) -> String {
    let window = Window {
        width: state.width,
        height: state.height,
        panes: state
            .panes
            .iter()
            .map(|p| Pane {
                geom: p.geom.clone(),
                grid: parse::grid_from_screen(p.parser.screen()),
            })
            .collect(),
    };
    render::render_fragment(&window, config, resolver)
}

fn spawn_control(target: Option<&str>) -> anyhow::Result<Child> {
    let mut cmd = Command::new("tmux");
    cmd.arg("-C").arg("attach-session");
    if let Some(t) = target {
        cmd.arg("-t").arg(t);
    }
    let child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(child)
}

/// A classified control-mode line. Borrows from the line buffer.
enum Event<'a> {
    Output { pane: &'a [u8], data: &'a [u8] },
    Resync,
    Exit,
    Ignore,
}

fn classify(line: &[u8]) -> Event<'_> {
    // `%output %<pane-id> <escaped-data>` — the only high-frequency message; parse
    // it on raw bytes since the data is UTF-8 with `\ooo` escapes for controls.
    if let Some(rest) = line.strip_prefix(b"%output ") {
        let sp = rest.iter().position(|&b| b == b' ').unwrap_or(rest.len());
        let pane = &rest[..sp];
        let data = rest.get(sp + 1..).unwrap_or(b"");
        return Event::Output { pane, data };
    }
    // Everything else is rare; a prefix check on the marker is enough.
    for m in [
        &b"%layout-change"[..],
        b"%window-add",
        b"%window-close",
        b"%unlinked-window-close",
        b"%window-pane-changed",
        b"%session-window-changed",
        b"%session-changed",
        b"%client-session-changed",
    ] {
        if line.starts_with(m) {
            return Event::Resync;
        }
    }
    if line.starts_with(b"%exit") {
        return Event::Exit;
    }
    Event::Ignore
}

/// Undo tmux control-mode escaping: control bytes and `\` are emitted as `\ooo`
/// (three octal digits); every other byte is literal (raw UTF-8 passes through).
fn unescape(b: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && i + 3 < b.len()
            && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c))
        {
            let v = u16::from(b[i + 1] - b'0') * 64
                + u16::from(b[i + 2] - b'0') * 8
                + u16::from(b[i + 3] - b'0');
            out.push(v as u8);
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    out
}

fn strip_eol(line: &mut Vec<u8>) {
    while matches!(line.last(), Some(b'\n' | b'\r')) {
        line.pop();
    }
}

use crate::render::banner;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unescape_octal_and_literals() {
        // `\033` -> ESC, literal text passes through, `\\` (\134) -> backslash.
        assert_eq!(unescape(b"\\033[1mA\\134B"), b"\x1b[1mA\\B");
        // Raw UTF-8 bytes (e.g. from a wide glyph) pass through untouched.
        assert_eq!(unescape("é".as_bytes()), "é".as_bytes());
    }

    #[test]
    fn classify_output_splits_pane_and_data() {
        match classify(b"%output %3 hello\\012") {
            Event::Output { pane, data } => {
                assert_eq!(pane, b"%3");
                assert_eq!(data, b"hello\\012");
            }
            _ => panic!("expected output"),
        }
    }

    #[test]
    fn classify_structural_and_exit() {
        assert!(matches!(classify(b"%layout-change @0 abc"), Event::Resync));
        assert!(matches!(
            classify(b"%window-pane-changed @0 %1"),
            Event::Resync
        ));
        assert!(matches!(classify(b"%exit"), Event::Exit));
        assert!(matches!(classify(b"%begin 123 0 1"), Event::Ignore));
    }
}

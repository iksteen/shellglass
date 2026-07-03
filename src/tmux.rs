//! Snapshot input source: shell out to `tmux` to read window/pane geometry and
//! capture each pane's rendered contents (with SGR escapes).

use crate::model::PaneGeom;
use anyhow::{Context, Result, anyhow, bail};
use std::process::Command;

/// Raw (unparsed) capture for one pane: its placement, the `capture-pane -e`
/// output bytes, and the pane cursor (col, row), 0-based. `capture-pane` carries
/// no cursor, so we track it separately and restore it when seeding the parser —
/// otherwise relative `%output` (e.g. interactive echo) lands at the wrong place.
pub struct RawPane {
    pub geom: PaneGeom,
    pub capture: String,
    pub cursor: (u16, u16),
}

/// A window snapshot before terminal parsing.
pub struct RawWindow {
    pub width: u16,
    pub height: u16,
    pub panes: Vec<RawPane>,
}

fn run(args: &[&str]) -> Result<String> {
    let out = Command::new("tmux")
        .args(args)
        .output()
        .context("failed to execute tmux (is it installed and on PATH?)")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!("tmux {:?} failed: {}", args, stderr.trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Capture the given target's active window: geometry + per-pane contents.
///
/// `target` is any tmux target (e.g. `session`, `session:window`); when `None`,
/// tmux's current window is used.
pub fn capture(target: Option<&str>) -> Result<RawWindow> {
    // Window size.
    let size_fmt = "#{window_width}x#{window_height}";
    let mut size_args = vec!["display-message", "-p"];
    if let Some(t) = target {
        size_args.push("-t");
        size_args.push(t);
    }
    size_args.push(size_fmt);
    let size = run(&size_args)?;
    let (width, height) = parse_size(size.trim())
        .ok_or_else(|| anyhow!("unexpected window size from tmux: {:?}", size.trim()))?;

    // Pane geometry + cursor. Fields are space-separated in a fixed order.
    let pane_fmt = "#{pane_id} #{pane_left} #{pane_top} #{pane_width} #{pane_height} #{pane_active} #{cursor_x} #{cursor_y}";
    let mut list_args = vec!["list-panes"];
    if let Some(t) = target {
        list_args.push("-t");
        list_args.push(t);
    }
    list_args.push("-F");
    list_args.push(pane_fmt);
    let listing = run(&list_args)?;

    let mut panes = Vec::new();
    for line in listing.lines().filter(|l| !l.trim().is_empty()) {
        let (geom, cursor) = parse_pane_line(line)
            .ok_or_else(|| anyhow!("unexpected list-panes line: {:?}", line))?;
        // Capture this pane with escape sequences (-e), to stdout (-p), and with
        // trailing spaces preserved (-N) so full-width colored bars (status
        // lines drawn via ESC[K background fill) keep their background to the
        // pane edge instead of being trimmed.
        let capture = run(&["capture-pane", "-e", "-N", "-p", "-t", &geom.id])?;
        panes.push(RawPane { geom, capture, cursor });
    }

    if panes.is_empty() {
        bail!("no panes found for target {:?}", target);
    }

    Ok(RawWindow {
        width,
        height,
        panes,
    })
}

fn parse_size(s: &str) -> Option<(u16, u16)> {
    let (w, h) = s.split_once('x')?;
    Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
}

fn parse_pane_line(line: &str) -> Option<(PaneGeom, (u16, u16))> {
    let mut it = line.split_whitespace();
    let id = it.next()?.to_string();
    let left = it.next()?.parse().ok()?;
    let top = it.next()?.parse().ok()?;
    let width = it.next()?.parse().ok()?;
    let height = it.next()?.parse().ok()?;
    let active = it.next()? == "1";
    let cursor_x = it.next()?.parse().ok()?;
    let cursor_y = it.next()?.parse().ok()?;
    Some((
        PaneGeom { id, left, top, width, height, active },
        (cursor_x, cursor_y),
    ))
}

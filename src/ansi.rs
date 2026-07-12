//! Pure `Grid`→ANSI renderer for the read-only SSH viewer ([`crate::ssh`]).
//!
//! No I/O lives here: [`paint`] maps a frame delta to the escape bytes that bring a
//! client terminal from `prev` to `next`, so every bit of terminal-output logic is
//! unit-testable without a russh handshake. The SSH loop just writes what this
//! returns and remembers `next` as the following call's `prev`.
//!
//! A viewer terminal smaller than the session gets a top-left crop plus a one-line
//! inverse status notice on its bottom row; a bigger one sees the whole screen.

use crate::model::{Color, Frame, Grid, StyledCell};
use std::fmt::Write;

/// ANSI bytes bringing a `view.0`×`view.1` (cols×rows) client terminal from `prev`
/// (`None` = unknown → full repaint) to `next`.
pub fn paint(prev: Option<&Frame>, next: &Frame, view: (u16, u16)) -> String {
    let Frame::Screen(g) = next;
    paint_screen(prev_screen(prev, g), g, view)
}

/// The previous grid iff it can serve as a diff base: same kind and same dimensions.
/// A kind or size change forces a full repaint (`None`).
fn prev_screen<'a>(prev: Option<&'a Frame>, next: &Grid) -> Option<&'a Grid> {
    match prev {
        Some(Frame::Screen(p)) if p.cols == next.cols && p.rows.len() == next.rows.len() => Some(p),
        _ => None,
    }
}

fn paint_screen(prev: Option<&Grid>, g: &Grid, view: (u16, u16)) -> String {
    let (vcols, vrows) = view;
    let cropped = g.cols > vcols || g.rows.len() > usize::from(vrows);
    // One row is reserved for the status notice when cropped.
    let content_rows = usize::from(if cropped {
        vrows.saturating_sub(1)
    } else {
        vrows
    });
    let shown = g.rows.len().min(content_rows);
    let full = prev.is_none();

    let mut out = String::new();
    if full {
        out.push_str("\x1b[2J\x1b[H");
    }
    for r in 0..shown {
        // ponytail: compare the full row even under crop — a change past view_cols
        // triggers a harmless extra repaint. Upgrade: per-span diff.
        if full || prev.is_some_and(|p| p.rows[r] != g.rows[r]) {
            paint_row(&mut out, r, &g.rows[r], vcols);
        }
    }
    // Status line depends only on dims (constant within a connection), so paint it
    // once per full repaint.
    if full && cropped {
        status_line(&mut out, g, view);
    }
    // DECSCUSR passes through so the client terminal shows the session's cursor
    // shape (vim's insert-mode bar); emitted on change, or on full when non-default.
    if g.cursor_style != prev.map_or(0, |p| p.cursor_style) {
        let _ = write!(out, "\x1b[{} q", g.cursor_style);
    }
    // The session title passes through (OSC 2); a cleared title emits the
    // empty string, which resets it.
    if g.title != prev.map_or("", |p| p.title.as_str()) {
        let _ = write!(out, "\x1b]2;{}\x1b\\", g.title);
    }
    // OSC 10/11 default-color overrides pass through too (reset: 110/111).
    let prev_defaults = prev.map_or((Color::Default, Color::Default), |p| p.default_colors);
    for (i, (now, before)) in [
        (g.default_colors.0, prev_defaults.0),
        (g.default_colors.1, prev_defaults.1),
    ]
    .into_iter()
    .enumerate()
    {
        if now != before {
            match now {
                Color::Rgb(r, g, b) => {
                    let _ = write!(out, "\x1b]{};#{r:02x}{g:02x}{b:02x}\x1b\\", 10 + i);
                }
                _ => {
                    let _ = write!(out, "\x1b]{}\x1b\\", 110 + i);
                }
            }
        }
    }
    cursor_seq(&mut out, g, view, content_rows);
    out
}

/// A row's cells, absolutely positioned and SGR-run-coalesced. The line is cleared
/// *before* painting (not with a trailing EL): a `\x1b[K` after a row that fills the
/// full view width erases the rightmost glyph, which autowrap parks in the deferred-
/// wrap column — the same hazard `status_line` dodges by truncating to `cols-1`.
fn paint_row(out: &mut String, r: usize, cells: &[StyledCell], vcols: u16) {
    let _ = write!(out, "\x1b[{};1H\x1b[0m\x1b[K", r + 1);
    let mut col: u16 = 0;
    let mut cur: Option<Style> = None; // None = SGR state unknown → first cell emits
    for cell in cells {
        if col >= vcols {
            break;
        }
        let w = if cell.wide { 2 } else { 1 };
        if col + w > vcols {
            // A wide cell straddling the crop boundary → one space (keeps alignment).
            out.push_str("\x1b[0m ");
            break;
        }
        let st = Style::of(cell);
        if cur.as_ref() != Some(&st) {
            out.push_str(&st.sgr());
            cur = Some(st);
        }
        out.push_str(if cell.text.is_empty() {
            " "
        } else {
            &cell.text
        });
        col += w;
    }
    // Reset so a trailing style doesn't bleed into the next line's leading clear.
    out.push_str("\x1b[0m");
}

fn status_line(out: &mut String, g: &Grid, view: (u16, u16)) {
    let (vcols, vrows) = view;
    let text = format!(
        "session {}x{} — your terminal {}x{} — read-only, q quits",
        g.cols,
        g.rows.len(),
        vcols,
        vrows
    );
    // ponytail: truncate to cols-1 so the last cell stays empty, dodging the
    // bottom-right deferred-wrap glitch.
    let text: String = text
        .chars()
        .take(usize::from(vcols.saturating_sub(1)))
        .collect();
    let _ = write!(out, "\x1b[{vrows};1H\x1b[K\x1b[7m{text}\x1b[0m");
}

/// Show + position the cursor when it's inside the visible (cropped) area, else hide.
fn cursor_seq(out: &mut String, g: &Grid, view: (u16, u16), content_rows: usize) {
    let (vcols, _) = view;
    match g.cursor {
        Some((r, c)) if usize::from(r) < content_rows && c < vcols => {
            let _ = write!(out, "\x1b[{};{}H\x1b[?25h", r + 1, c + 1);
        }
        _ => out.push_str("\x1b[?25l"),
    }
}

/// The SGR-relevant styling of a cell (`wide` is layout, not styling).
#[derive(PartialEq)]
struct Style {
    fg: Color,
    bg: Color,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: u8,
    strike: bool,
    ulcolor: Color,
    inverse: bool,
}

impl Style {
    fn of(c: &StyledCell) -> Style {
        Style {
            fg: c.fg,
            bg: c.bg,
            bold: c.bold,
            dim: c.dim,
            italic: c.italic,
            underline: c.underline,
            strike: c.strike,
            ulcolor: c.ulcolor,
            inverse: c.inverse,
        }
    }

    /// One reset-then-set sequence (`\x1b[0;…m`) — no attribute-removal tracking, so
    /// each emission fully specifies the cell's style regardless of what preceded it.
    fn sgr(&self) -> String {
        let mut p = String::from("\x1b[0");
        for (set, code) in [
            (self.bold, "1"),
            (self.dim, "2"),
            (self.italic, "3"),
            (self.strike, "9"),
            (self.inverse, "7"),
        ] {
            if set {
                p.push(';');
                p.push_str(code);
            }
        }
        // Plain `4` for single underline (maximally compatible); the kitty
        // `4:n` subparam form only for the fancy styles.
        match self.underline {
            0 => {}
            1 => p.push_str(";4"),
            n => {
                let _ = write!(p, ";4:{n}");
            }
        }
        if self.ulcolor != Color::Default {
            push_ulcolor(&mut p, self.ulcolor);
        }
        push_color(&mut p, self.fg, false);
        push_color(&mut p, self.bg, true);
        p.push('m');
        p
    }
}

/// Append an SGR 58 underline-color parameter (no idx-form shorthands exist).
fn push_ulcolor(p: &mut String, c: Color) {
    match c {
        Color::Default => {}
        Color::Idx(i) => {
            let _ = write!(p, ";58;5;{i}");
        }
        Color::Rgb(r, g, b) => {
            let _ = write!(p, ";58;2;{r};{g};{b}");
        }
    }
}

/// Append an SGR color parameter: base/bright ANSI for `Idx(0..=15)`, 256-color for
/// the rest, truecolor for `Rgb`. `Default` adds nothing (the leading `0` reset covers it).
fn push_color(p: &mut String, c: Color, bg: bool) {
    match c {
        Color::Default => {}
        Color::Idx(i @ 0..=7) => {
            let base: u16 = if bg { 40 } else { 30 };
            let _ = write!(p, ";{}", base + u16::from(i));
        }
        Color::Idx(i @ 8..=15) => {
            let base: u16 = if bg { 100 } else { 90 };
            let _ = write!(p, ";{}", base + u16::from(i - 8));
        }
        Color::Idx(i) => {
            let _ = write!(p, ";{};5;{i}", if bg { 48 } else { 38 });
        }
        Color::Rgb(r, g, b) => {
            let _ = write!(p, ";{};2;{r};{g};{b}", if bg { 48 } else { 38 });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::grid_from_capture;

    fn screen(rows: &[&str], cols: u16) -> Frame {
        let joined = rows.join("\n");
        Frame::Screen(grid_from_capture(&joined, cols, rows.len() as u16))
    }

    #[test]
    fn sgr_maps_base_bright_256_and_rgb_colors() {
        // fg base(1)+bright(9)+256(200) and bg base(2)+256(240)+rgb.
        let cases = [
            (Color::Idx(1), false, ";31"),
            (Color::Idx(9), false, ";91"),
            (Color::Idx(200), false, ";38;5;200"),
            (Color::Idx(2), true, ";42"),
            (Color::Idx(240), true, ";48;5;240"),
            (Color::Rgb(10, 20, 30), false, ";38;2;10;20;30"),
            (Color::Rgb(1, 2, 3), true, ";48;2;1;2;3"),
            (Color::Default, false, ""),
        ];
        for (c, bg, want) in cases {
            let mut p = String::new();
            push_color(&mut p, c, bg);
            assert_eq!(p, want, "{c:?} bg={bg}");
        }
    }

    #[test]
    fn attrs_render_and_reset_between_runs() {
        // Bold "A", then plain "B": the plain cell must re-emit a bare reset so the
        // bold doesn't bleed.
        let f = screen(&["\x1b[1mA\x1b[0mB"], 4);
        let out = paint(None, &f, (4, 1));
        assert!(out.contains("\x1b[0;1mA"), "bold set: {out:?}");
        assert!(out.contains("\x1b[0mB"), "reset before plain B: {out:?}");
    }

    #[test]
    fn full_paint_clears_and_positions_every_row() {
        let f = screen(&["ab", "cd"], 2);
        let out = paint(None, &f, (2, 2));
        assert!(out.starts_with("\x1b[2J\x1b[H"), "clears on full: {out:?}");
        assert!(out.contains("\x1b[1;1H"), "row 0 positioned");
        assert!(out.contains("\x1b[2;1H"), "row 1 positioned");
    }

    #[test]
    fn full_width_row_keeps_last_glyph() {
        // A row filling the exact view width must not be followed by a trailing EL —
        // that would erase the rightmost glyph parked in the deferred-wrap column.
        let f = screen(&["ABCD"], 4);
        let out = paint(None, &f, (4, 1));
        assert!(
            out.contains("ABCD\x1b[0m"),
            "last glyph kept, reset not EL: {out:?}"
        );
        assert!(
            !out.contains("D\x1b[K"),
            "no trailing EL after the last glyph: {out:?}"
        );
        assert!(
            out.contains("\x1b[1;1H\x1b[0m\x1b[K"),
            "line cleared before painting: {out:?}"
        );
    }

    #[test]
    fn row_diff_repaints_only_changed_rows() {
        let a = screen(&["ab", "cd"], 2);
        let b = screen(&["ab", "cX"], 2);
        let out = paint(Some(&a), &b, (2, 2));
        assert!(
            !out.starts_with("\x1b[2J"),
            "no full clear on diff: {out:?}"
        );
        assert!(!out.contains("\x1b[1;1H"), "row 0 unchanged, not repainted");
        assert!(out.contains("\x1b[2;1H"), "row 1 repainted");
    }

    #[test]
    fn resize_forces_full_repaint() {
        // Different dims ⇒ prev is unusable as a base even though kinds match.
        let a = screen(&["ab"], 2);
        let b = screen(&["abc"], 3);
        let out = paint(Some(&a), &b, (3, 1));
        assert!(out.starts_with("\x1b[2J\x1b[H"), "resize → full: {out:?}");
    }

    #[test]
    fn wide_cells_advance_two_columns() {
        // "世" is wide (2 cols), then "x": x must land at column 3.
        let f = screen(&["世x"], 4);
        let out = paint(None, &f, (4, 1));
        assert!(out.contains("世x"), "wide glyph then x: {out:?}");
    }

    #[test]
    fn wide_cell_at_crop_boundary_becomes_space() {
        // View is 1 col wide (2 rows so a content row survives the status line); the
        // single wide glyph can't fit → a space, no glyph.
        let f = screen(&["世"], 2);
        let out = paint(None, &f, (1, 2));
        assert!(
            !out.contains('世'),
            "wide glyph dropped at boundary: {out:?}"
        );
        assert!(out.contains("\x1b[0m "), "replaced by a space: {out:?}");
    }

    #[test]
    fn crop_shows_top_left_with_status_line() {
        // 8-col session on a 6-col / 3-row terminal (wide enough for the status text):
        // 2 content rows + the status on row 3.
        let f = screen(&["abcdefgh", "row2here", "row3here", "row4here"], 8);
        let out = paint(None, &f, (60, 3));
        assert!(out.contains("\x1b[1;1H"), "top row painted");
        assert!(
            out.contains("\x1b[3;1H\x1b[K\x1b[7m"),
            "inverse status on bottom row: {out:?}"
        );
        assert!(
            out.contains("session 8x4 — your terminal 60x3"),
            "status text: {out:?}"
        );
    }

    #[test]
    fn no_status_line_when_session_fits() {
        let f = screen(&["ab", "cd"], 2);
        let out = paint(None, &f, (10, 10));
        assert!(
            !out.contains("\x1b[7m"),
            "no inverse status when it fits: {out:?}"
        );
    }

    #[test]
    fn cursor_shown_iff_visible_and_inside_crop() {
        let mut f = screen(&["abcd", "efgh", "ijkl"], 4);
        let Frame::Screen(g) = &mut f;
        // Cursor at (2,0) is below the single visible content row of a 3x2 view.
        g.cursor = Some((2, 0));
        let out = paint(None, &f, (3, 2));
        assert!(
            out.contains("\x1b[?25l") && !out.contains("\x1b[?25h"),
            "hidden below crop: {out:?}"
        );
        // Cursor at (0,1) is inside → shown at 1;2.
        let Frame::Screen(g) = &mut f;
        g.cursor = Some((0, 1));
        let out = paint(None, &f, (3, 2));
        assert!(
            out.contains("\x1b[1;2H\x1b[?25h"),
            "shown inside crop: {out:?}"
        );
    }
}

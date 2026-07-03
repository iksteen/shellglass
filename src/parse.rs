//! Turn `capture-pane -e` output into a parser-agnostic [`Grid`] by driving a
//! throwaway `vt100` terminal emulator.

use crate::model::{Color, Grid, StyledCell};
use std::fmt::Write as _;

/// Build a persistent vt100 parser seeded with a `capture-pane -e` snapshot, with
/// the cursor placed at `cursor` (col, row), 0-based. The returned parser is then
/// fed incremental control-mode `%output` bytes as the live pane produces them.
pub fn seed_parser(capture: &str, cols: u16, rows: u16, cursor: (u16, u16)) -> vt100::Parser {
    let mut parser = vt100::Parser::new(rows, cols, 0);

    // capture-pane separates lines with '\n' and has no cursor motion. Feed a
    // CR+LF between lines so vt100 returns to column 0 each row, and crucially do
    // NOT emit a trailing newline (which would scroll the top line away).
    let lines: Vec<&str> = capture.trim_end_matches('\n').split('\n').collect();
    let feed = lines.join("\r\n");
    parser.process(feed.as_bytes());

    // capture-pane carries neither the cursor position nor the active pen (SGR);
    // feeding the snapshot leaves the cursor at the end of the last line and the
    // pen as that cell's attributes. Restore the real cursor, and set the pen to
    // the cell there — the rendition a program writing at the cursor would
    // inherit — so relative `%output` (a shell/irssi echoing keystrokes) continues
    // with the right attributes instead of a stale bottom-right background.
    let (col, row) = cursor;
    let pen = {
        let screen = parser.screen();
        screen
            .cell(row, col)
            .map_or_else(|| "\x1b[m".to_string(), pen_sgr)
    };
    parser.process(format!("{pen}\x1b[{};{}H", row + 1, col + 1).as_bytes());
    parser
}

/// Reconstruct an SGR sequence (leading reset) reproducing `cell`'s pen.
fn pen_sgr(cell: &vt100::Cell) -> String {
    let mut s = String::from("\x1b[0");
    match cell.fgcolor() {
        vt100::Color::Default => {}
        vt100::Color::Idx(i) => {
            let _ = write!(s, ";38;5;{i}");
        }
        vt100::Color::Rgb(r, g, b) => {
            let _ = write!(s, ";38;2;{r};{g};{b}");
        }
    }
    match cell.bgcolor() {
        vt100::Color::Default => {}
        vt100::Color::Idx(i) => {
            let _ = write!(s, ";48;5;{i}");
        }
        vt100::Color::Rgb(r, g, b) => {
            let _ = write!(s, ";48;2;{r};{g};{b}");
        }
    }
    if cell.bold() {
        s.push_str(";1");
    }
    if cell.dim() {
        s.push_str(";2");
    }
    if cell.italic() {
        s.push_str(";3");
    }
    if cell.underline() {
        s.push_str(";4");
    }
    if cell.inverse() {
        s.push_str(";7");
    }
    s.push('m');
    s
}

/// Render `capture` (SGR-annotated text) into a fixed `cols`×`rows` grid. Only the
/// tests exercise this one-shot form; live code seeds then feeds a parser instead.
#[cfg(test)]
pub fn grid_from_capture(capture: &str, cols: u16, rows: u16) -> Grid {
    grid_from_screen(seed_parser(capture, cols, rows, (0, 0)).screen())
}

/// Extract a parser-agnostic [`Grid`] from a live vt100 screen (the long-lived
/// control-mode screen at render time; also used by the capture-seeding tests).
pub fn grid_from_screen(screen: &vt100::Screen) -> Grid {
    let (srows, scols) = screen.size();

    let mut grid_rows: Vec<Vec<StyledCell>> = Vec::with_capacity(srows as usize);
    for r in 0..srows {
        let mut row = Vec::with_capacity(scols as usize);
        let mut c = 0;
        while c < scols {
            let Some(cell) = screen.cell(r, c) else {
                c += 1;
                continue;
            };
            if cell.is_wide_continuation() {
                // Belongs to the preceding wide cell; skip.
                c += 1;
                continue;
            }
            let wide = cell.is_wide();
            row.push(StyledCell {
                text: cell.contents().to_string(),
                fg: conv_color(cell.fgcolor()),
                bg: conv_color(cell.bgcolor()),
                bold: cell.bold(),
                dim: cell.dim(),
                italic: cell.italic(),
                underline: cell.underline(),
                inverse: cell.inverse(),
                wide,
            });
            c += if wide { 2 } else { 1 };
        }
        grid_rows.push(row);
    }

    let cursor = if screen.hide_cursor() {
        None
    } else {
        Some(screen.cursor_position())
    };

    Grid {
        cols: scols,
        rows: grid_rows,
        cursor,
    }
}

fn conv_color(c: vt100::Color) -> Color {
    match c {
        vt100::Color::Default => Color::Default,
        vt100::Color::Idx(i) => Color::Idx(i),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Color;

    #[test]
    fn seed_restores_pen_from_cursor_cell() {
        // Row 0 is plain; row 1 has a green (idx 2) background.
        let cap = "abc\n\x1b[42mXYZ";

        // Cursor on a default cell (0,0): a following char stays default.
        let mut p = seed_parser(cap, 10, 2, (0, 0));
        p.process(b"Q");
        assert_eq!(
            p.screen().cell(0, 0).unwrap().bgcolor(),
            vt100::Color::Default,
            "pen should be default on a default cell"
        );

        // Cursor on the green cell (0,1): a following char inherits the bg.
        let mut p2 = seed_parser(cap, 10, 2, (0, 1));
        p2.process(b"Q");
        assert_eq!(
            p2.screen().cell(1, 0).unwrap().bgcolor(),
            vt100::Color::Idx(2),
            "pen should inherit the cursor cell's background"
        );
    }

    #[test]
    fn attrs_and_colors() {
        // Bold red "A", then a truecolor "B", then plain "C".
        let cap = "\x1b[1;31mA\x1b[0m\x1b[38;2;10;20;30mB\x1b[0mC";
        let g = grid_from_capture(cap, 10, 1);
        let row = &g.rows[0];
        assert_eq!(row[0].text, "A");
        assert!(row[0].bold);
        assert_eq!(row[0].fg, Color::Idx(1));
        assert_eq!(row[1].fg, Color::Rgb(10, 20, 30));
        assert!(!row[1].bold);
        assert_eq!(row[2].text, "C");
        assert_eq!(row[2].fg, Color::Default);
    }

    #[test]
    fn wide_char_collapses_continuation() {
        // A CJK ideograph occupies two columns; we keep one wide cell.
        let g = grid_from_capture("世x", 10, 1);
        let row = &g.rows[0];
        assert_eq!(row[0].text, "世");
        assert!(row[0].wide);
        assert_eq!(row[1].text, "x");
        assert!(!row[1].wide);
    }

    #[test]
    fn no_top_line_scroll() {
        // Two lines must both survive (regression: trailing newline scrolling).
        let g = grid_from_capture("top\nbot", 10, 2);
        assert_eq!(g.rows[0][0].text, "t");
        assert_eq!(g.rows[1][0].text, "b");
    }
}

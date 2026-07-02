//! Turn `capture-pane -e` output into a parser-agnostic [`Grid`] by driving a
//! throwaway `vt100` terminal emulator.

use crate::model::{Color, Grid, Pane, StyledCell, Window};
use crate::tmux::RawWindow;

/// Parse a whole window snapshot.
pub fn parse_window(raw: RawWindow) -> Window {
    let panes = raw
        .panes
        .into_iter()
        .map(|p| Pane {
            grid: grid_from_capture(&p.capture, p.geom.width, p.geom.height),
            geom: p.geom,
        })
        .collect();
    Window {
        width: raw.width,
        height: raw.height,
        panes,
    }
}

/// Render `capture` (SGR-annotated text) into a fixed `cols`×`rows` grid.
pub fn grid_from_capture(capture: &str, cols: u16, rows: u16) -> Grid {
    let mut parser = vt100::Parser::new(rows, cols, 0);

    // capture-pane separates lines with '\n' and has no cursor motion. Feed a
    // CR+LF between lines so vt100 returns to column 0 each row, and crucially do
    // NOT emit a trailing newline (which would scroll the top line away).
    let lines: Vec<&str> = capture.trim_end_matches('\n').split('\n').collect();
    let feed = lines.join("\r\n");
    parser.process(feed.as_bytes());

    let screen = parser.screen();
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
        cols,
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

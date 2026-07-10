//! Extract a parser-agnostic [`Grid`] from a live `vt100` screen — the boundary
//! where the PTY backend's terminal emulation becomes renderer-ready cells.

use crate::model::{Color, Grid, StyledCell};

/// Build a `cols`×`rows` grid from SGR-annotated text by driving a throwaway vt100
/// emulator. Lines are joined with CR+LF (so vt100 returns to column 0 each row)
/// and no trailing newline (which would scroll the top line away). Test-only: live
/// code extracts from the long-lived PTY screen via [`grid_from_screen`].
#[cfg(test)]
pub fn grid_from_capture(capture: &str, cols: u16, rows: u16) -> Grid {
    let mut parser = vt100::Parser::new(rows, cols, 0);
    let feed = capture
        .trim_end_matches('\n')
        .split('\n')
        .collect::<Vec<_>>()
        .join("\r\n");
    parser.process(feed.as_bytes());
    grid_from_screen(parser.screen())
}

/// Extract a parser-agnostic [`Grid`] from a live vt100 screen (the long-lived
/// PTY-fed screen at render time; also used by the tests below). Generic over
/// the screen's cell-data slot — extraction reads only the rendered state;
/// the PTY backend resolves its image tags separately (`resolve_images`).
pub fn grid_from_screen<T>(screen: &vt100::Screen<T>) -> Grid {
    let (srows, scols) = screen.size();

    let mut grid_rows: Vec<Vec<StyledCell>> = Vec::with_capacity(srows as usize);
    // OSC 8: the id→URI table for the ids actually on screen, built during
    // the same scan (ids are stable, so entries never change once inserted).
    let mut links = std::collections::BTreeMap::new();
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
            // Canonicalize blank cells to a space: the renderers draw them
            // identically (style rides separately), so keeping the distinction
            // only costs wire bytes and spurious erase-vs-space diffs.
            let mut text = cell.contents().to_string();
            if text.is_empty() {
                text.push(' ');
            }
            // A link id whose URI was pruned from the parser's bounded table
            // renders unlinked rather than dangling.
            let link = cell.link().and_then(|id| {
                let uri = screen.link_uri(id)?;
                links.entry(id.get()).or_insert_with(|| uri.to_string());
                Some(id.get())
            });
            row.push(StyledCell {
                text,
                fg: conv_color(cell.fgcolor()),
                bg: conv_color(cell.bgcolor()),
                bold: cell.bold(),
                dim: cell.dim(),
                italic: cell.italic(),
                underline: cell.underline_style(),
                strike: cell.strikethrough(),
                concealed: cell.concealed(),
                blink: cell.blink(),
                ulcolor: conv_color(cell.ulcolor()),
                inverse: cell.inverse(),
                link,
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
        cursor_style: screen.cursor_style(),
        default_colors: (
            screen
                .default_fg()
                .map_or(Color::Default, |(r, g, b)| Color::Rgb(r, g, b)),
            screen
                .default_bg()
                .map_or(Color::Default, |(r, g, b)| Color::Rgb(r, g, b)),
        ),
        title: screen.title().to_string(),
        links,
        // Images are tracked outside vt100 (it drops the sequences); the PTY backend
        // fills this in after extraction. Text-only extraction leaves it empty.
        images: Vec::new(),
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
    fn modern_sgr_extracts() {
        // Undercurl with a colored underline, then strikethrough.
        let g = grid_from_capture("\x1b[4:3;58;5;196mA\x1b[0m\x1b[9mB", 10, 1);
        let row = &g.rows[0];
        assert_eq!(row[0].underline, 3);
        assert_eq!(row[0].ulcolor, Color::Idx(196));
        assert!(!row[0].strike);
        assert_eq!(row[1].underline, 0);
        assert!(row[1].strike);
    }

    #[test]
    fn hyperlinks_extract_ids_and_table() {
        let g = grid_from_capture(
            "\x1b]8;;https://example.com\x1b\\hi\x1b]8;;\x1b\\ there",
            20,
            1,
        );
        let row = &g.rows[0];
        let id = row[0].link.expect("linked");
        assert_eq!(row[1].link, Some(id));
        assert_eq!(row[2].link, None);
        assert_eq!(
            g.links.get(&id).map(String::as_str),
            Some("https://example.com")
        );
        assert_eq!(g.links.len(), 1);
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

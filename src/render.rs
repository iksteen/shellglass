//! Grid → HTML. Panes are absolutely positioned in cell units; within a pane,
//! adjacent cells with identical styling (including resolved font) are coalesced
//! into a single `<span>`.

use crate::config::Config;
use crate::fonts::Resolver;
use crate::model::{Color, StyledCell, Window};
use std::fmt::Write as _;

const DEFAULT_FG: (u8, u8, u8) = (0xd0, 0xd0, 0xd0);
const DEFAULT_BG: (u8, u8, u8) = (0x00, 0x00, 0x00);

/// Everything that goes inside `<style>`: embedded `@font-face` plus the config-
/// derived base CSS. Computed by whoever owns the config (the standalone server or
/// a push client); the hub just stores and re-emits it, so it renders nothing.
pub fn head_css(font_css: &str, config: &Config) -> String {
    let base_css = format!(
        "html,body {{ margin:0; background:#000; }}\n\
         #screen {{ font-family:{stack}; font-size:{fs}px; --lh:{lh}px; \
         line-height:var(--lh); color:{fg}; }}\n\
         .screen {{ position:relative; }}\n\
         .pane {{ position:absolute; white-space:pre; overflow:hidden; \
         box-sizing:border-box; }}\n\
         .pane.active {{ box-shadow: inset 0 0 0 1px #3b3b3b; }}\n\
         .row {{ height:var(--lh); }}\n\
         .run {{ display:inline-block; height:var(--lh); vertical-align:top; \
         overflow:hidden; }}\n",
        stack = font_stack(config),
        fs = config.font_size_px,
        lh = config.line_height_px(),
        fg = hex(DEFAULT_FG),
    );
    format!("{font_css}{base_css}")
}

/// Assemble the full page from a ready `<style>` body, the initial `#screen`
/// fragment, and the updater `<script>`.
pub fn page(head_css: &str, fragment: &str, script: &str) -> String {
    format!(
        "<!doctype html>\n<html><head><meta charset=\"utf-8\">\n\
         <title>tmuxsnitch</title>\n<style>\n{head_css}</style>\n</head>\n\
         <body>\n<div id=\"screen\">{fragment}</div>\n\
         <script>\n{script}\n</script>\n\
         </body></html>",
    )
}

/// SSE updater: subscribe to `events_path` and swap `#screen` on each push.
/// EventSource auto-reconnects if the stream drops, so no retry logic here.
pub fn sse_script(events_path: &str) -> String {
    format!(
        "const es = new EventSource('{events_path}');\n\
         es.onmessage = e => {{ document.getElementById('screen').innerHTML = e.data; }};"
    )
}

/// Standalone page (local tmux → local viewer): streams live from `/events`.
pub fn render_page(fragment: &str, font_css: &str, config: &Config) -> String {
    page(&head_css(font_css, config), fragment, &sse_script("/events"))
}

/// Red in-page error banner (shared by every mode's failure path).
pub fn banner(msg: &str) -> String {
    let esc = msg.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    format!(
        "<div style=\"color:#ff6b6b;font-family:monospace;padding:8px;\">\
         tmuxsnitch: {esc}</div>"
    )
}

/// Render just the panes (the swappable inner fragment the poller replaces).
pub fn render_fragment(window: &Window, config: &Config, resolver: &Resolver) -> String {
    let mut out = String::new();
    let _ = write!(
        out,
        "<div class=\"screen\" style=\"width:{}ch;height:calc({} * var(--lh));\">",
        window.width, window.height
    );
    for pane in &window.panes {
        let g = &pane.geom;
        let active = if g.active { " active" } else { "" };
        let _ = write!(
            out,
            "<div class=\"pane{active}\" style=\"left:{}ch;top:calc({} * var(--lh));\
             width:{}ch;height:calc({} * var(--lh));\">",
            g.left, g.top, g.width, g.height
        );
        render_pane_body(&mut out, pane, config, resolver);
        out.push_str("</div>");
    }
    out.push_str("</div>");
    out
}

fn render_pane_body(out: &mut String, pane: &crate::model::Pane, config: &Config, resolver: &Resolver) {
    let cursor = pane.grid.cursor;
    for (r, row) in pane.grid.rows.iter().enumerate() {
        let cursor_col = match cursor {
            Some((cr, cc)) if cr as usize == r => Some(cc),
            _ => None,
        };
        out.push_str("<div class=\"row\">");
        render_row(out, row, cursor_col, config, resolver);
        out.push_str("</div>");
    }
}

fn render_row(
    out: &mut String,
    row: &[StyledCell],
    cursor_col: Option<u16>,
    config: &Config,
    resolver: &Resolver,
) {
    let mut col: u16 = 0;
    // Accumulator for a run of plain (base-font) text cells.
    let mut run_style: Option<String> = None;
    let mut cols: u16 = 0;
    let mut text = String::new();

    for cell in row {
        let is_cursor = cursor_col == Some(col);
        let w = if cell.wide { 2 } else { 1 };

        // Symbol cells (font override) render as a scaled SVG in their own box;
        // they never coalesce, so flush any pending text run first.
        if let Some(font) = cell_font(cell, resolver, config) {
            flush_text_run(out, &run_style, cols, &mut text);
            run_style = None;
            cols = 0;
            emit_symbol_cell(out, cell, is_cursor, w, &font);
        } else {
            let style = cell_box_style(cell, is_cursor);
            if run_style.as_ref() != Some(&style) {
                flush_text_run(out, &run_style, cols, &mut text);
                run_style = Some(style);
                cols = 0;
            }
            let ch = if cell.text.is_empty() { " " } else { &cell.text };
            escape_into(&mut text, ch);
            cols += w;
        }
        col += w;
    }
    flush_text_run(out, &run_style, cols, &mut text);
}

/// Emit a run of plain text as one fixed-width `inline-block` box that occupies
/// exactly `cols` base-font columns.
fn flush_text_run(out: &mut String, style: &Option<String>, cols: u16, text: &mut String) {
    if text.is_empty() {
        return;
    }
    let s = style.as_deref().unwrap_or("");
    let _ = write!(out, "<span class=\"run\" style=\"width:{cols}ch;{s}\">{text}</span>");
    text.clear();
}

/// Emit a symbol cell: an SVG-scaled glyph inside a fixed-width box. Nerd-Font
/// glyphs are designed for a ~1em-square cell, far wider than our base font's
/// `ch`; rendering them natively clips/misaligns them. Scaling via SVG matches
/// what kitty does: powerline/box separators **stretch** to fill the cell (so
/// they tile seamlessly), other icons **fit** proportionally and centered.
fn emit_symbol_cell(out: &mut String, cell: &StyledCell, is_cursor: bool, w: u16, font: &str) {
    let box_style = cell_box_style(cell, is_cursor);
    let first = cell.text.chars().next().unwrap_or(' ');
    let par = if is_fill_glyph(first) {
        "none"
    } else {
        "xMidYMid meet"
    };
    let mut glyph = String::new();
    escape_into(&mut glyph, &cell.text);
    // viewBox is the glyph's ~1em design box (advance≈em); preserveAspectRatio
    // maps it onto the actual cell box (width:{w}ch × --lh).
    let _ = write!(
        out,
        "<span class=\"run\" style=\"width:{w}ch;{box_style}\">\
         <svg viewBox=\"0 0 14 14\" preserveAspectRatio=\"{par}\" \
         style=\"display:block;width:100%;height:100%\">\
         <text x=\"0\" y=\"12\" font-family=\"{font}\" font-size=\"14\" \
         fill=\"currentColor\">{glyph}</text></svg></span>"
    );
}

/// Powerline separators and box-drawing glyphs must fill the whole cell so
/// adjacent segments tile without gaps; everything else scales proportionally.
fn is_fill_glyph(c: char) -> bool {
    ('\u{E0B0}'..='\u{E0D4}').contains(&c)
}

/// Resolved override font stack for a cell, or `None` to use the base font.
fn cell_font(cell: &StyledCell, resolver: &Resolver, config: &Config) -> Option<String> {
    let first = cell.text.chars().next()?;
    let fam = resolver.font_for(first)?;
    Some(format!("{},{}", quote_family(fam), font_stack(config)))
}

/// Box CSS for a cell (colors, weight, style, decoration) — no font-family; the
/// override font lives on the inner span so it never changes the box's `ch`.
fn cell_box_style(cell: &StyledCell, is_cursor: bool) -> String {
    let mut fg = resolve_rgb(cell.fg);
    let mut bg = resolve_rgb(cell.bg);

    // Reverse video swaps fg/bg, materializing defaults.
    if cell.inverse ^ is_cursor {
        let f = fg.unwrap_or(DEFAULT_FG);
        let b = bg.unwrap_or(DEFAULT_BG);
        fg = Some(b);
        bg = Some(f);
    }
    if cell.dim {
        let f = fg.unwrap_or(DEFAULT_FG);
        fg = Some((f.0 / 10 * 6, f.1 / 10 * 6, f.2 / 10 * 6));
    }

    let mut s = String::new();
    if let Some(c) = fg {
        let _ = write!(s, "color:{};", hex(c));
    }
    if let Some(c) = bg {
        let _ = write!(s, "background:{};", hex(c));
    }
    if cell.bold {
        s.push_str("font-weight:bold;");
    }
    if cell.italic {
        s.push_str("font-style:italic;");
    }
    if cell.underline {
        s.push_str("text-decoration:underline;");
    }
    s
}

/// The base font stack: the configured default family with a `monospace` last
/// resort (unless the default already is the `monospace` generic).
fn font_stack(config: &Config) -> String {
    let d = quote_family(&config.default_font);
    if config.default_font == "monospace" {
        d
    } else {
        format!("{d},monospace")
    }
}

fn resolve_rgb(c: Color) -> Option<(u8, u8, u8)> {
    match c {
        Color::Default => None,
        Color::Idx(i) => Some(palette(i)),
        Color::Rgb(r, g, b) => Some((r, g, b)),
    }
}

fn hex((r, g, b): (u8, u8, u8)) -> String {
    format!("#{r:02x}{g:02x}{b:02x}")
}

/// Quote a font family unless it's a CSS generic keyword.
fn quote_family(name: &str) -> String {
    const GENERICS: [&str; 5] = ["monospace", "serif", "sans-serif", "cursive", "fantasy"];
    if GENERICS.contains(&name) {
        name.to_string()
    } else {
        format!("'{}'", name.replace('\\', "\\\\").replace('\'', "\\'"))
    }
}

fn escape_into(out: &mut String, s: &str) {
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(ch),
        }
    }
}

/// xterm 256-color palette.
fn palette(i: u8) -> (u8, u8, u8) {
    const BASE16: [(u8, u8, u8); 16] = [
        (0x00, 0x00, 0x00),
        (0xcd, 0x00, 0x00),
        (0x00, 0xcd, 0x00),
        (0xcd, 0xcd, 0x00),
        (0x00, 0x00, 0xee),
        (0xcd, 0x00, 0xcd),
        (0x00, 0xcd, 0xcd),
        (0xe5, 0xe5, 0xe5),
        (0x7f, 0x7f, 0x7f),
        (0xff, 0x00, 0x00),
        (0x00, 0xff, 0x00),
        (0xff, 0xff, 0x00),
        (0x5c, 0x5c, 0xff),
        (0xff, 0x00, 0xff),
        (0x00, 0xff, 0xff),
        (0xff, 0xff, 0xff),
    ];
    match i {
        0..=15 => BASE16[i as usize],
        16..=231 => {
            let n = i - 16;
            let levels = [0u8, 95, 135, 175, 215, 255];
            let r = levels[(n / 36) as usize];
            let g = levels[((n / 6) % 6) as usize];
            let b = levels[(n % 6) as usize];
            (r, g, b)
        }
        232..=255 => {
            let v = 8 + 10 * (i - 232);
            (v, v, v)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, SymbolMap};
    use crate::model::{Grid, Pane, PaneGeom};
    use crate::parse::grid_from_capture;

    fn window_from(cap: &str, w: u16, h: u16) -> Window {
        let mut grid = grid_from_capture(cap, w, h);
        grid.cursor = None; // these tests check cell styling, not cursor rendering
        Window {
            width: w,
            height: h,
            panes: vec![Pane {
                geom: PaneGeom { id: "%0".into(), left: 0, top: 0, width: w, height: h, active: true },
                grid,
            }],
        }
    }

    #[test]
    fn colors_and_bold_render() {
        let cfg = Config::default();
        let res = Resolver::build(&cfg).unwrap();
        let html = render_fragment(&window_from("\x1b[1;31mA\x1b[0mB", 4, 1), &cfg, &res);
        assert!(html.contains("font-weight:bold;"), "bold missing: {html}");
        assert!(html.contains("color:#cd0000;"), "red missing: {html}");
        assert!(html.contains(">A</span>"));
    }

    #[test]
    fn symbol_map_overrides_font() {
        let mut cfg = Config::default();
        cfg.default_font = "Menlo".into();
        cfg.symbol_map = vec![SymbolMap {
            ranges: vec!["U+E0A0-U+E0D4".into()],
            font: "Symbols Nerd Font".into(),
        }];
        let res = Resolver::build(&cfg).unwrap();
        // U+E0A0 (Powerline branch) is an icon-like glyph -> SVG, proportional fit.
        let html = render_fragment(&window_from("\u{E0A0}", 2, 1), &cfg, &res);
        assert!(
            html.contains("font-family=\"'Symbols Nerd Font','Menlo',monospace\""),
            "override font missing: {html}"
        );
        assert!(
            html.contains("preserveAspectRatio=\"xMidYMid meet\""),
            "icon glyph should fit proportionally: {html}"
        );
    }

    #[test]
    fn glyph_locked_to_one_cell() {
        // A powerline glyph must occupy exactly a 1ch box so the rest of the
        // line keeps its columns (this is what fixes right-aligned segments).
        let mut cfg = Config::default();
        cfg.symbol_map = vec![SymbolMap {
            ranges: vec!["U+E0B0".into()],
            font: "Symbols Nerd Font Mono".into(),
        }];
        let res = Resolver::build(&cfg).unwrap();
        // glyph + "ab", padded to 10 columns.
        let html = render_fragment(&window_from("\u{E0B0}ab", 10, 1), &cfg, &res);
        // The glyph is a 1ch box holding a stretch-to-fill SVG (E0B0 is a separator)...
        assert!(
            html.contains("<span class=\"run\" style=\"width:1ch;\"><svg viewBox=\"0 0 14 14\" preserveAspectRatio=\"none\""),
            "separator glyph not stretch-filled in a 1ch box: {html}"
        );
        // ...and the run widths across the row sum to exactly the 10 columns, so
        // the glyph's real advance can't shift anything after it.
        let row = html.split("<div class=\"row\">").nth(1).unwrap();
        let sum: u16 = row
            .split("width:")
            .skip(1)
            .filter_map(|s| s.split("ch").next()?.parse::<u16>().ok())
            .sum();
        assert_eq!(sum, 10, "run widths don't tile the row: {html}");
    }

    #[test]
    fn absolute_pane_geometry() {
        let cfg = Config::default();
        let res = Resolver::build(&cfg).unwrap();
        let w = Window {
            width: 80,
            height: 24,
            panes: vec![Pane {
                geom: PaneGeom { id: "%1".into(), left: 41, top: 0, width: 39, height: 24, active: false },
                grid: Grid { cols: 39, rows: vec![vec![]], cursor: None },
            }],
        };
        let html = render_fragment(&w, &cfg, &res);
        assert!(html.contains("left:41ch;"), "pane offset missing: {html}");
        assert!(html.contains("width:39ch;"));
    }
}

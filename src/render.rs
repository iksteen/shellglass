//! Grid → HTML. Rows are stacked in the `#screen` box; within a row, adjacent cells
//! with identical styling (including resolved font) are coalesced into a single
//! `<span>`, each absolutely positioned at its column.

use crate::config::Config;
use crate::fonts::{FontFile, Resolver};
use crate::model::{Color, Grid, StyledCell};
use serde::Serialize;
use std::fmt::Write as _;

/// The compiled browser renderer (`viewer/viewer.ts` → JS), baked into the binary
/// and served verbatim at `/viewer.js` by both the standalone server and the hub.
/// `build.rs` produces it (via `tsc` when present, else the committed
/// `viewer/dist/viewer.js`).
pub const VIEWER_JS: &str = include_str!(concat!(env!("OUT_DIR"), "/viewer.js"));

/// `@font-face` blocks for served fonts. Each references `{url_prefix}{index}`,
/// where the index is the font's position (matching [`crate::fonts::font_assets`])
/// — the standalone server serves those at `/fonts/…`, the hub per session at
/// `/s/<id>/fonts/…`. Serving (not inlining) keeps the page small and lets the
/// browser cache the font.
pub fn font_face_css(fonts: &[FontFile], url_prefix: &str) -> String {
    let mut css = String::new();
    for (i, f) in fonts.iter().enumerate() {
        let _ = writeln!(
            css,
            "@font-face {{ font-family:'{}'; font-weight:{}; src:url(\"{}{}\") format('{}'); }}",
            crate::fonts::css_escape_family(&f.family),
            if f.bold { "bold" } else { "normal" },
            url_prefix,
            i,
            f.format,
        );
    }
    css
}

const DEFAULT_FG: (u8, u8, u8) = (0xd0, 0xd0, 0xd0);
const DEFAULT_BG: (u8, u8, u8) = (0x00, 0x00, 0x00);

/// Built-in viewer template (n3o-style dark chrome). A template is a full HTML
/// document with three tokens the renderer fills: `{{style}}` (the generated
/// terminal CSS + `@font-face`), `{{screen}}` (the `#screen` div the SSE updater
/// swaps), and `{{script}}` (that updater). Override via `--config`'s `template`.
pub const DEFAULT_TEMPLATE: &str = include_str!("template.html");

/// Built-in CRT theme: the default chrome plus a pure-CSS CRT overlay (scanlines,
/// phosphor bloom, flicker, vignette). Selected with `theme = "crt"`.
pub const CRT_TEMPLATE: &str = include_str!("template-crt.html");

/// Everything that goes inside `<style>`: the served-font `@font-face` rules plus
/// the config-derived base CSS. Computed by whoever owns the config (the standalone
/// server or a push client); the hub just stores and re-emits it, so it renders
/// nothing.
pub fn head_css(font_css: &str, config: &Config) -> String {
    // The terminal backdrop lives on #screen (not body) so a template controls the
    // surrounding page background; #screen stays black wherever it's placed.
    let base_css = format!(
        "html,body {{ margin:0; }}\n\
         #screen {{ font-family:{stack}; font-size:{fs}px; --lh:{lh}px; \
         line-height:var(--lh); color:{fg}; background:#000; }}\n\
         .screen {{ position:relative; white-space:pre; overflow:hidden; }}\n\
         .row {{ position:relative; height:var(--lh); }}\n\
         .run {{ position:absolute; top:0; height:var(--lh); overflow:hidden; }}\n",
        stack = font_stack(config),
        fs = config.font_size_px,
        lh = config.line_height_px(),
        fg = hex(DEFAULT_FG),
    );
    format!("{font_css}{base_css}")
}

/// Fill a viewer `template` with the terminal `<style>`, the `#screen` fragment,
/// and the updater `<script>`. Tokens: `{{style}}`, `{{screen}}`, `{{script}}`.
/// `{{screen}}` is filled last and its replacement is never re-scanned, so a
/// literal `{{script}}` the user typed into the terminal can't corrupt the page.
pub fn page(template: &str, head_css: &str, fragment: &str, script: &str) -> String {
    // `script` already carries its own `<script>` tags (it loads the external
    // renderer), so inject it raw rather than wrapping it.
    template
        .replace("{{style}}", &format!("<style>\n{head_css}</style>"))
        .replace("{{script}}", script)
        .replace(
            "{{screen}}",
            &format!("<div id=\"screen\">{fragment}</div>"),
        )
}

/// The page's updater block: a tiny inline config the renderer reads (the SSE path
/// plus the render config), then a `<script src>` for the baked `/viewer.js`. The
/// renderer itself is served as a cacheable file, not inlined. `cfg_json` is a JSON
/// object (from [`render_config_json`], or the client's, relayed by the hub); an
/// empty string falls back to `null` so the renderer uses its built-in defaults.
pub fn sse_script(events_path: &str, cfg_json: &str) -> String {
    let cfg = if cfg_json.is_empty() {
        "null"
    } else {
        cfg_json
    };
    let events = serde_json::to_string(events_path).unwrap_or_else(|_| "\"/events\"".into());
    // The inline classic script runs at parse time (setting the config); the module
    // script is deferred by default and runs after, so the config is ready. viewer.js
    // is an ES module, hence type="module".
    format!(
        "<script>window.SHELLGLASS={{events:{events},cfg:{cfg}}};</script>\n\
         <script type=\"module\" src=\"/viewer.js\"></script>"
    )
}

/// Render config handed to the browser renderer so it resolves colors and symbol
/// fonts exactly as the Rust reference would: default fg/bg, the base stack for
/// stretch-fill glyphs, and the `symbol_map` overrides as `[lo, hi, familyStack]`
/// (each stack pre-joined like [`cell_font`] builds it). Injected once per page.
pub fn render_config_json(config: &Config, resolver: &Resolver) -> String {
    #[derive(Serialize)]
    struct RenderConfig {
        #[serde(rename = "defFg")]
        def_fg: String,
        #[serde(rename = "defBg")]
        def_bg: String,
        #[serde(rename = "fillFont")]
        fill_font: String,
        sym: Vec<(u32, u32, String)>,
    }
    let stack = font_stack(config);
    let sym = resolver
        .entries()
        .iter()
        .map(|(r, fam)| {
            (
                *r.start(),
                *r.end(),
                format!("{},{}", quote_family(fam), stack),
            )
        })
        .collect();
    let cfg = RenderConfig {
        def_fg: hex(DEFAULT_FG),
        def_bg: hex(DEFAULT_BG),
        fill_font: stack,
        sym,
    };
    serde_json::to_string(&cfg).expect("RenderConfig serializes")
}

/// Standalone page (local command → local viewer): streams live from `/events`.
pub fn render_page(
    template: &str,
    fragment: &str,
    font_css: &str,
    config: &Config,
    cfg_json: &str,
) -> String {
    page(
        template,
        &head_css(font_css, config),
        fragment,
        &sse_script("/events", cfg_json),
    )
}

/// Red in-page error banner (shared by every mode's failure path).
pub fn banner(msg: &str) -> String {
    let esc = msg
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    format!(
        "<div style=\"color:#ff6b6b;font-family:monospace;padding:8px;\">\
         shellglass: {esc}</div>"
    )
}

/// Render the screen (the swappable inner fragment each SSE update replaces): a
/// `.screen` box of absolutely-sized rows. The browser renderer (`viewer.ts`)
/// reproduces this exact structure, so the two must stay in lockstep.
pub fn render_fragment(grid: &Grid, config: &Config, resolver: &Resolver) -> String {
    let mut out = String::new();
    let _ = write!(
        out,
        "<div class=\"screen\" style=\"width:{}ch;height:calc({} * var(--lh));\">",
        grid.cols,
        grid.rows.len()
    );
    let cursor = grid.cursor;
    for (r, row) in grid.rows.iter().enumerate() {
        let cursor_col = match cursor {
            Some((cr, cc)) if cr as usize == r => Some(cc),
            _ => None,
        };
        out.push_str("<div class=\"row\">");
        render_row(&mut out, row, cursor_col, config, resolver);
        out.push_str("</div>");
    }
    out.push_str("</div>");
    out
}

fn render_row(
    out: &mut String,
    row: &[StyledCell],
    cursor_col: Option<u16>,
    config: &Config,
    resolver: &Resolver,
) {
    let mut col: u16 = 0;
    // Accumulator for a run of plain (base-font) text cells, with the column it
    // starts at — every run is positioned absolutely at `left:{run_col}ch` so its
    // x is `round(run_col × ch)`, identical on every row. (Stacking inline-blocks
    // instead sums each run's independently-rounded `ch` width, so the sub-pixel
    // error accumulates and a column — e.g. a tmux pane divider — drifts per row.)
    let mut run_style: Option<String> = None;
    let mut run_col: u16 = 0;
    let mut cols: u16 = 0;
    let mut text = String::new();

    for cell in row {
        let is_cursor = cursor_col == Some(col);
        let w = if cell.wide { 2 } else { 1 };

        // Symbol cells (font override) render as a scaled SVG in their own box;
        // they never coalesce, so flush any pending text run first.
        if let Some(font) = svg_font(cell, resolver, config) {
            flush_text_run(out, &run_style, run_col, cols, &mut text);
            run_style = None;
            cols = 0;
            emit_symbol_cell(out, cell, is_cursor, col, w, &font);
        } else {
            let style = cell_box_style(cell, is_cursor);
            if run_style.as_ref() != Some(&style) {
                flush_text_run(out, &run_style, run_col, cols, &mut text);
                run_style = Some(style);
                cols = 0;
            }
            if cols == 0 {
                run_col = col; // first cell of this run fixes its left edge
            }
            let ch = if cell.text.is_empty() {
                " "
            } else {
                &cell.text
            };
            escape_into(&mut text, ch);
            cols += w;
        }
        col += w;
    }
    flush_text_run(out, &run_style, run_col, cols, &mut text);
}

/// Emit a run of plain text as one fixed-width `inline-block` box that occupies
/// exactly `cols` base-font columns.
fn flush_text_run(
    out: &mut String,
    style: &Option<String>,
    col: u16,
    cols: u16,
    text: &mut String,
) {
    if text.is_empty() {
        return;
    }
    let s = style.as_deref().unwrap_or("");
    let _ = write!(
        out,
        "<span class=\"run\" style=\"left:{col}ch;width:{cols}ch;{s}\">{text}</span>"
    );
    text.clear();
}

/// Emit a symbol cell: an SVG-scaled glyph inside a fixed-width box. Nerd-Font
/// glyphs are designed for a ~1em-square cell, far wider than our base font's
/// `ch`; rendering them natively clips/misaligns them. Scaling via SVG matches
/// what kitty does: powerline/box separators **stretch** to fill the cell (so
/// they tile seamlessly), other icons **fit** proportionally and centered.
fn emit_symbol_cell(
    out: &mut String,
    cell: &StyledCell,
    is_cursor: bool,
    col: u16,
    w: u16,
    font: &str,
) {
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
        "<span class=\"run\" style=\"left:{col}ch;width:{w}ch;{box_style}\">\
         <svg viewBox=\"0 0 14 14\" preserveAspectRatio=\"{par}\" \
         style=\"display:block;width:100%;height:100%\">\
         <text x=\"0\" y=\"12\" font-family=\"{font}\" font-size=\"14\" \
         fill=\"currentColor\">{glyph}</text></svg></span>"
    );
}

/// Powerline separators, box-drawing lines, and block elements must fill the whole
/// cell so adjacent segments tile without gaps — the page's `line-height` > 1 gives
/// a plain-text `│` vertical leading between rows, so a stack of them (e.g. a tmux
/// pane divider) renders dashed. Everything else scales proportionally.
fn is_fill_glyph(c: char) -> bool {
    matches!(c,
        '\u{E0B0}'..='\u{E0D4}'     // powerline separators
        | '\u{2500}'..='\u{259F}'   // box drawing + block elements
        | '\u{1FB00}'..='\u{1FBAF}' // legacy computing: sextants, eighth-blocks, wedges
    )
    // ponytail: braille (U+2800–28FF) is deliberately out — a dot matrix that must
    // scale proportionally, not stretch. Octants (U+1CD00+) omitted until a font/tool
    // in the wild needs them; base fonts rarely carry the glyphs, so SVG'ing them
    // would just stretch tofu.
}

/// Font stack for rendering `cell` as a scaled SVG glyph, or `None` for plain text.
/// A `symbol_map` override wins. Otherwise *fill* glyphs still need SVG cell-locking
/// even with no `symbol_map` — powerline separators are wider than a cell and clip;
/// box-drawing lines gap vertically under `line-height` > 1 — so route them through
/// the base fallback stack (the browser picks whichever family has the glyph).
fn svg_font(cell: &StyledCell, resolver: &Resolver, config: &Config) -> Option<String> {
    if let Some(font) = cell_font(cell, resolver, config) {
        return Some(font);
    }
    let first = cell.text.chars().next()?;
    is_fill_glyph(first).then(|| font_stack(config))
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

/// The base font stack: the configured families in order, with a `monospace`
/// last resort appended unless already present. The browser resolves each glyph
/// against this stack, giving Kitty-style per-character fallback for free.
fn font_stack(config: &Config) -> String {
    let mut fams: Vec<String> = config
        .default_font
        .iter()
        .map(|f| quote_family(f))
        .collect();
    if !config.default_font.iter().any(|f| f == "monospace") {
        fams.push("monospace".to_string());
    }
    fams.join(",")
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
// Tests build a Config then tweak a field or two — the mutate-after-default form
// reads better here than struct-update with ..Default::default().
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use crate::config::{Config, SymbolMap};
    use crate::parse::grid_from_capture;

    fn grid_from(cap: &str, w: u16, h: u16) -> Grid {
        let mut grid = grid_from_capture(cap, w, h);
        grid.cursor = None; // these tests check cell styling, not cursor rendering
        grid
    }

    #[test]
    fn colors_and_bold_render() {
        let cfg = Config::default();
        let res = Resolver::build(&cfg).unwrap();
        let html = render_fragment(&grid_from("\x1b[1;31mA\x1b[0mB", 4, 1), &cfg, &res);
        assert!(html.contains("font-weight:bold;"), "bold missing: {html}");
        assert!(html.contains("color:#cd0000;"), "red missing: {html}");
        assert!(html.contains(">A</span>"));
    }

    #[test]
    fn symbol_map_overrides_font() {
        let mut cfg = Config::default();
        cfg.default_font = vec!["Menlo".into()];
        cfg.symbol_map = vec![SymbolMap {
            ranges: vec!["U+E0A0-U+E0D4".into()],
            font: "Symbols Nerd Font".into(),
        }];
        let res = Resolver::build(&cfg).unwrap();
        // U+E0A0 (Powerline branch) is an icon-like glyph -> SVG, proportional fit.
        let html = render_fragment(&grid_from("\u{E0A0}", 2, 1), &cfg, &res);
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
        let html = render_fragment(&grid_from("\u{E0B0}ab", 10, 1), &cfg, &res);
        // The glyph is a 1ch box holding a stretch-to-fill SVG (E0B0 is a separator)...
        assert!(
            html.contains("<span class=\"run\" style=\"left:0ch;width:1ch;\"><svg viewBox=\"0 0 14 14\" preserveAspectRatio=\"none\""),
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
    fn box_drawing_divider_stretch_fills() {
        // A tmux pane divider is a column of `│` (U+2502). Rendered as plain text
        // it gaps vertically under line-height > 1; it must go through the same
        // stretch-to-fill SVG path as powerline separators so it reads solid.
        let cfg = Config::default();
        let res = Resolver::build(&cfg).unwrap();
        let html = render_fragment(&grid_from("\u{2502}", 2, 1), &cfg, &res);
        assert!(
            html.contains("preserveAspectRatio=\"none\""),
            "box-drawing divider not stretch-filled: {html}"
        );
        // A legacy-computing sextant (U+1FB00) tiles like a block element too.
        let sextant = render_fragment(&grid_from("\u{1FB00}", 2, 1), &cfg, &res);
        assert!(
            sextant.contains("preserveAspectRatio=\"none\""),
            "legacy sextant not stretch-filled: {sextant}"
        );
    }

    #[test]
    fn runs_are_positioned_at_their_column() {
        // Each run's `left` is its absolute column, not its flow position — so a
        // column (e.g. a divider) lands at the same x on every row regardless of
        // what precedes it. Here `ab│` puts the divider at column 2.
        let cfg = Config::default();
        let res = Resolver::build(&cfg).unwrap();
        let html = render_fragment(&grid_from("ab\u{2502}", 5, 1), &cfg, &res);
        assert!(
            html.contains("style=\"left:0ch;width:2ch;"),
            "leading text run not at column 0: {html}"
        );
        assert!(
            html.contains("style=\"left:2ch;width:1ch;"),
            "divider not positioned at its column: {html}"
        );
    }

    #[test]
    fn font_list_becomes_a_css_fallback_stack() {
        // A multi-family default_font emits every family in order (browser does
        // per-glyph fallback), with monospace appended as last resort.
        let mut cfg = Config::default();
        cfg.default_font = vec!["Menlo".into(), "Symbols Nerd Font Mono".into()];
        let css = head_css("", &cfg);
        assert!(
            css.contains("font-family:'Menlo','Symbols Nerd Font Mono',monospace;"),
            "font stack missing/ordered wrong: {css}"
        );
    }

    #[test]
    fn powerline_separator_svg_scaled_without_symbol_map() {
        // With no symbol_map (the default fallback setup), a fill separator must
        // still go through the stretch-to-fill SVG path — otherwise its glyph is
        // clipped to a block. It renders in a 1ch box with a base fallback stack.
        let cfg = Config::default();
        let res = Resolver::build(&cfg).unwrap();
        let html = render_fragment(&grid_from("\u{E0B0}", 2, 1), &cfg, &res);
        assert!(
            html.contains("preserveAspectRatio=\"none\""),
            "separator not SVG stretch-filled: {html}"
        );
        assert!(
            html.contains("<svg viewBox=\"0 0 14 14\""),
            "no SVG glyph box: {html}"
        );
        // A plain letter next to it stays plain text (not SVG).
        let plain = render_fragment(&grid_from("a", 2, 1), &cfg, &res);
        assert!(
            !plain.contains("<svg"),
            "plain text should not be SVG: {plain}"
        );
    }

    #[test]
    fn page_fills_template_tokens() {
        let tmpl = "<head>{{style}}</head><body>{{screen}}{{script}}</body>";
        // Fragment contains a literal token the user "typed" — it must survive
        // verbatim (screen is filled last and never re-scanned).
        let html = page(tmpl, "CSS", "hi {{script}} there", "<script>JS</script>");
        assert!(html.contains("<style>\nCSS</style>"), "{html}");
        assert!(
            html.contains("<div id=\"screen\">hi {{script}} there</div>"),
            "{html}"
        );
        // The script block is injected raw (it carries its own <script> tags).
        assert!(html.contains("<script>JS</script>"), "{html}");
        // The typed {{script}} token in the fragment was NOT expanded into a script.
        assert_eq!(
            html.matches("<script>").count(),
            1,
            "typed token got expanded: {html}"
        );
    }

    #[test]
    fn builtin_templates_have_all_tokens() {
        for (name, tmpl) in [("default", DEFAULT_TEMPLATE), ("crt", CRT_TEMPLATE)] {
            for tok in ["{{style}}", "{{screen}}", "{{script}}"] {
                assert!(tmpl.contains(tok), "{name} template missing {tok}");
            }
        }
    }

    #[test]
    fn font_face_css_references_served_urls() {
        let fonts = vec![FontFile {
            family: "NF".into(),
            mime: "font/ttf",
            format: "truetype",
            bytes: vec![1, 2],
            bold: false,
        }];
        let css = font_face_css(&fonts, "/s/abc/fonts/");
        assert!(css.contains("font-family:'NF'"), "{css}");
        assert!(
            css.contains("src:url(\"/s/abc/fonts/0\") format('truetype')"),
            "{css}"
        );
    }

    #[test]
    fn fragment_sizes_the_screen_box() {
        let cfg = Config::default();
        let res = Resolver::build(&cfg).unwrap();
        let html = render_fragment(&grid_from("ab", 5, 1), &cfg, &res);
        assert!(
            html.starts_with(
                "<div class=\"screen\" style=\"width:5ch;height:calc(1 * var(--lh));\">"
            ),
            "screen box not sized: {html}"
        );
        assert!(html.contains("<div class=\"row\">"), "no row: {html}");
    }
}

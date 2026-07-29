//! Page assembly: the viewer template, its `<style>`/`<script>` blocks, the render
//! config handed to the browser, and the baked renderer/favicon assets. All cell
//! rendering lives in `viewer/viewer.ts` — the page ships with an empty `#screen`
//! and the renderer paints it from the full frame that heads every SSE stream.

// The Config/Resolver-consuming assembly is mirror-side (serve/push build the
// page + render config from local state). The hub also uses `font_face_rule` to
// turn structured uploaded font metadata into CSS; it never accepts font CSS.
#[cfg(feature = "presentation")]
use crate::config::Config;
use crate::fonts::FontFile;
#[cfg(feature = "presentation")]
use crate::fonts::Resolver;
#[cfg(feature = "presentation")]
use serde::Serialize;
#[cfg(feature = "presentation")]
use std::collections::HashMap;

/// The compiled browser renderer (`viewer/viewer.ts` → JS), baked into the binary
/// and served verbatim at `/viewer.js` by both the standalone server and the hub.
/// `build.rs` produces it (via `tsc` when present, else the committed
/// `viewer/dist/viewer.js`).
pub const VIEWER_JS: &str = include_str!(concat!(env!("OUT_DIR"), "/viewer.js"));

pub const FAVICON_SVG: &str = include_str!("favicon.svg");

/// Short content tag of the baked renderer, the second half of the page-reload
/// version pair: the wire proto can be unchanged while viewer.js itself changes
/// (a render fix) — a mismatch on either reloads the page. Hashing the bytes
/// means there is no hand-maintained JS version to forget to bump.
pub fn viewer_tag() -> &'static str {
    use std::hash::{Hash, Hasher};
    static TAG: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    TAG.get_or_init(|| {
        let mut h = std::hash::DefaultHasher::new();
        VIEWER_JS.hash(&mut h);
        format!("{:016x}", h.finish())
    })
}

/// `@font-face` blocks for served fonts. Each references `{url_prefix}{key}`,
/// where the key is the font's content address ([`crate::proto::content_key`]) —
/// the same hash the hub derives from the pushed bytes, so an honest client's
/// URLs land on its own fonts by construction. The prefix is the
/// page-RELATIVE `fonts/` everywhere: it resolves against the
/// directory-shaped page URL (`/` standalone, `/s/<slug>/` hub), for any slug
/// and behind any subpath mount. Serving (not inlining) keeps the page small
/// and lets the browser cache the font (forever — the URL is immutable).
pub fn font_face_css(fonts: &[FontFile], url_prefix: &str) -> String {
    let mut css = String::new();
    for f in fonts {
        css.push_str(&font_face_rule(
            &f.family,
            f.bold,
            url_prefix,
            &f.key(),
            f.format,
        ));
    }
    css
}

/// One hub/serve-generated `@font-face` rule. Every free-form field has already
/// been reduced to data: `family` is CSS+HTML escaped here, while `key` and
/// `format` are hub-derived allowlisted tokens in hub mode.
pub fn font_face_rule(
    family: &str,
    bold: bool,
    url_prefix: &str,
    key: &str,
    format: &str,
) -> String {
    format!(
        "@font-face {{ font-family:'{}'; font-weight:{}; src:url(\"{}{}\") format('{}'); }}\n",
        crate::fonts::css_escape_family(family),
        if bold { "bold" } else { "normal" },
        url_prefix,
        key,
        format,
    )
}

#[cfg(feature = "presentation")]
const DEFAULT_FG: (u8, u8, u8) = (0xd0, 0xd0, 0xd0);
#[cfg(feature = "presentation")]
const DEFAULT_BG: (u8, u8, u8) = (0x00, 0x00, 0x00);

/// Built-in viewer template (n3o-style dark chrome).
/// A template is a full HTML document with three tokens
/// the renderer fills: `{{style}}` (the generated terminal CSS + `@font-face`),
/// `{{screen}}` (the `#screen` div the live renderer fills), and `{{script}}`
/// (the scripts that boot it). Override via `--config`'s `template`.
pub const DEFAULT_TEMPLATE: &str = include_str!("template.html");

/// Built-in embed template (`?embed` on any view route): no chrome, terminal
/// scaled to fill the frame. Always used for embeds — a pusher's custom
/// template never applies, so an embed's look is predictable for host pages.
pub const EMBED_TEMPLATE: &str = include_str!("template_embed.html");

/// The stable embedding shim served at `/embed.js`: a classic script that
/// replaces its own `<script data-src="…">` tag with a `<shellglass-view>`
/// element (also usable directly in markup). By default the terminal renders
/// IFRAME-LESS, straight into the host page (light DOM) — `mode="shadow"` for a
/// style-isolated shadow root, `mode="iframe"` for the classic sandboxed frame
/// onto `?embed`. Deliberately version-agnostic; the live rendering lives in
/// `viewer.js`, imported per element.
pub const EMBED_JS: &str = include_str!("embed.js");

/// Everything that goes inside `<style>`: optional served-font `@font-face` rules
/// plus config-derived base CSS. Standalone serve passes its locally generated
/// rules; a push client passes an empty string and the hub prepends rules generated
/// from the uploaded structured font metadata.
#[cfg(feature = "presentation")]
pub fn head_css(font_css: &str, config: &Config) -> String {
    head_css_with_font_aliases(font_css, config, &HashMap::new())
}

/// Hub-mode base CSS. Every located configured family is replaced by the
/// validated wire identity the hub will use in its generated `@font-face` rules.
#[cfg(feature = "presentation")]
pub fn head_css_with_font_aliases(
    font_css: &str,
    config: &Config,
    aliases: &HashMap<String, String>,
) -> String {
    // The terminal backdrop lives on #screen (not body) so a template controls the
    // surrounding page background; #screen stays black wherever it's placed.
    // text-size-adjust: iOS Safari inflates the used font-size of wide text
    // blocks (phones in landscape especially) — the ghost rows are its prime
    // target, and #screen is fit-content sized BY those rows, so the boost
    // widens the whole terminal box (~2x aspect distortion) while the px
    // line-height pins row height, and the canvas glyphs land ~2x too big
    // for the grid. 100% opts the terminal out without disabling user zoom.
    format!(
        "{font_css}{}",
        terminal_base_css(
            &font_stack(config, aliases),
            config.font_size_px,
            config.line_height_px(),
        )
    )
}

/// Hub-owned presentation for every embed mode. The pusher contributes no CSS
/// or render configuration: only verified regular-face hashes select fonts.
/// All other presentation values are fixed hub literals.
pub fn hub_embed_head_css(font_css: &str, family_keys: &[String]) -> String {
    format!(
        "{font_css}{}",
        terminal_base_css(&hub_font_stack(family_keys), 14.0, 16.8)
    )
}

fn terminal_base_css(stack: &str, font_px: f32, line_height_px: f32) -> String {
    format!(
        "html,body {{ margin:0; }}\n\
         #screen {{ font-family:{stack}; font-size:{fs}px; --lh:{lh}px; \
         line-height:var(--lh); color:{fg}; background:#000; \
         -webkit-text-size-adjust:100%; text-size-adjust:100%; }}\n\
         .screen {{ position:relative; white-space:pre; overflow:hidden; }}\n\
         .row {{ position:relative; height:var(--lh); contain:layout style; }}\n",
        fs = font_px,
        lh = line_height_px,
        fg = "#d0d0d0",
    )
}

/// The base terminal CSS with nothing but built-in defaults — no config, no
/// served fonts. The hub serves this for a registered session whose pusher has
/// never connected (the operator-offline placeholder): custom fonts are
/// deliberately ignored until the pusher provides them, and the hub-only build
/// has no `Config` anyway. Keep the structure in lockstep with [`head_css`].
pub fn default_head_css() -> String {
    terminal_base_css("monospace", 14.0, 16.8)
}

/// Render config matching [`default_head_css`] — the viewer's own defaults,
/// spelled out (field names are viewer.ts's `Cfg`).
pub const DEFAULT_RENDER_CFG: &str = r##"{"defFg":"#d0d0d0","defBg":"#000000","fillFont":"monospace","fontPx":14,"lhPx":16.8,"sym":[]}"##;

/// Render config matching [`hub_embed_head_css`]. It is assembled entirely from
/// hub literals and verified regular-face hashes; no pushed JSON is reproduced.
pub fn hub_embed_render_config_json(family_keys: &[String]) -> String {
    let stack = serde_json::to_string(&hub_font_stack(family_keys))
        .expect("a generated font stack serializes");
    format!(
        r##"{{"defFg":"#d0d0d0","defBg":"#000000","fillFont":{stack},"fontPx":14,"lhPx":16.8,"sym":[]}}"##
    )
}

fn hub_font_stack(family_keys: &[String]) -> String {
    let mut stack = family_keys
        .iter()
        .map(|key| format!("'{}'", crate::fonts::css_escape_family(key)))
        .collect::<Vec<_>>();
    stack.push("monospace".to_string());
    stack.join(",")
}

/// Fill a viewer `template` with the terminal `<style>`, the (empty) `#screen`
/// div, and the updater `<script>`. Tokens: `{{style}}`, `{{screen}}`,
/// `{{script}}`. The screen starts empty — the renderer paints it from the full
/// frame that heads every SSE stream, one round-trip after load.
pub fn page(template: &str, head_css: &str, script: &str) -> String {
    // `script` already carries its own `<script>` tags (it loads the external
    // renderer), so inject it raw rather than wrapping it.
    template
        .replace("{{style}}", &format!("<style>\n{head_css}</style>"))
        .replace("{{script}}", script)
        .replace("{{screen}}", "<div id=\"screen\"></div>")
}

/// The page's updater block: a tiny inline config the renderer reads (the SSE path
/// plus the render config), then a `<script src>` for the baked renderer. The
/// renderer itself is served as a cacheable file, not inlined. `cfg_json` is a JSON
/// object (from [`render_config_json`], or the client's, relayed by the hub); an
/// empty string falls back to `null` so the renderer uses its built-in defaults.
///
/// Every URL here is RELATIVE (`events`, `viewer.js`) and resolves against the
/// page's directory-shaped URL (`/` standalone, `/s/<slug>/` on the hub): the
/// hub may sit behind a reverse proxy that mounts it under a subpath, which
/// neither it nor the client can know — absolute paths would escape the mount.
pub fn sse_script(events_path: &str, cfg_json: &str) -> String {
    let cfg = if cfg_json.is_empty() {
        "null"
    } else {
        cfg_json
    };
    let events = serde_json::to_string(events_path).unwrap_or_else(|_| "\"events\"".into());
    // The inline classic script runs at parse time (setting the config); the module
    // script is deferred by default and runs after, so the config is ready. viewer.js
    // is an ES module, hence type="module". `proto`/`js` are this binary's wire
    // version and baked-renderer content tag: the SSE stream re-announces both on
    // every (re)connect, and the page reloads itself on mismatch (server upgraded
    // under a loaded page — new wire format OR just a new viewer.js).
    let proto = crate::diff::PROTO;
    let js = viewer_tag();
    format!(
        "<script>window.SHELLGLASS={{events:{events},cfg:{cfg},proto:{proto},js:\"{js}\"}};</script>\n\
         <script type=\"module\" src=\"viewer.js?v={js}\"></script>"
    )
}

/// The boot object an iframe-less embed fetches (`GET config`), the JSON form of
/// the `window.SHELLGLASS` the baked page inlines: SSE path, render config, wire
/// proto and viewer tag. `events_path` is page-relative (`events`); the embed
/// resolves it against the session base. `cfg_json` is [`render_config_json`] or
/// the client's, relayed by the hub.
pub fn config_json(events_path: &str, cfg_json: &str) -> String {
    let cfg = if cfg_json.is_empty() {
        "null"
    } else {
        cfg_json
    };
    let events = serde_json::to_string(events_path).unwrap_or_else(|_| "\"events\"".into());
    format!(
        "{{\"events\":{events},\"cfg\":{cfg},\"proto\":{proto},\"js\":\"{js}\"}}",
        proto = crate::diff::PROTO,
        js = viewer_tag(),
    )
}

/// Render config handed to the browser renderer: default fg/bg, the base stack for
/// stretch-fill glyphs, the cell font-size / line-height (an iframe-less embed sets
/// them on its own container — the baked page gets them from the head CSS), and the
/// `symbol_map` overrides as `[lo, hi, familyStack]` (each stack pre-joined,
/// override family first). Injected once per page.
#[cfg(feature = "presentation")]
pub fn render_config_json(config: &Config, resolver: &Resolver) -> String {
    render_config_json_with_font_aliases(config, resolver, &HashMap::new())
}

/// Hub-mode render config, using the same content-hash family aliases as
/// [`head_css_with_font_aliases`] for base, symbol-map, and no-boost families.
#[cfg(feature = "presentation")]
pub fn render_config_json_with_font_aliases(
    config: &Config,
    resolver: &Resolver,
    aliases: &HashMap<String, String>,
) -> String {
    #[derive(Serialize)]
    struct RenderConfig {
        #[serde(rename = "defFg")]
        def_fg: String,
        #[serde(rename = "defBg")]
        def_bg: String,
        #[serde(rename = "fillFont")]
        fill_font: String,
        #[serde(rename = "fontPx")]
        font_px: f32,
        #[serde(rename = "lhPx")]
        lh_px: f32,
        sym: Vec<(u32, u32, String)>,
        /// Families whose `[fonts]` entry set `weight_boost = false` — the viewer
        /// skips the double-draw for cells whose primary family is one of these.
        /// Omitted when none, so the common config carries nothing extra.
        #[serde(rename = "noBoost", skip_serializing_if = "Vec::is_empty")]
        no_boost: Vec<String>,
    }
    let stack = font_stack(config, aliases);
    let sym = resolver
        .entries()
        .iter()
        .map(|(r, fam)| {
            (
                *r.start(),
                *r.end(),
                format!("{},{}", quote_family(fam, aliases), stack),
            )
        })
        .collect();
    let mut no_boost: Vec<String> = config
        .fonts
        .iter()
        .filter(|(_, f)| f.weight_boost == Some(false))
        .map(|(k, _)| aliases.get(k).unwrap_or(k).clone())
        .collect();
    no_boost.sort(); // HashMap order is nondeterministic; keep the emitted JSON stable
    let cfg = RenderConfig {
        def_fg: hex(DEFAULT_FG),
        def_bg: hex(DEFAULT_BG),
        fill_font: stack,
        font_px: config.font_size_px,
        lh_px: config.line_height_px(),
        sym,
        no_boost,
    };
    serde_json::to_string(&cfg).expect("RenderConfig serializes")
}

/// Standalone page (local command → local viewer): streams live from `events`
/// (relative — the page lives at `/`).
#[cfg(feature = "presentation")]
pub fn render_page(template: &str, font_css: &str, config: &Config, cfg_json: &str) -> String {
    page(
        template,
        &head_css(font_css, config),
        &sse_script("events", cfg_json),
    )
}

/// The base font stack: the configured families in order, with a `monospace`
/// last resort appended unless already present. The browser resolves each glyph
/// against this stack, giving Kitty-style per-character fallback for free.
#[cfg(feature = "presentation")]
fn font_stack(config: &Config, aliases: &HashMap<String, String>) -> String {
    let mut fams: Vec<String> = config
        .default_font
        .iter()
        .map(|f| quote_family(f, aliases))
        .collect();
    if !config.default_font.iter().any(|f| f == "monospace") {
        fams.push("monospace".to_string());
    }
    fams.join(",")
}

#[cfg(feature = "presentation")]
fn hex((r, g, b): (u8, u8, u8)) -> String {
    format!("#{r:02x}{g:02x}{b:02x}")
}

/// Quote a font family unless it's a CSS generic keyword.
#[cfg(feature = "presentation")]
fn quote_family(name: &str, aliases: &HashMap<String, String>) -> String {
    const GENERICS: [&str; 5] = ["monospace", "serif", "sans-serif", "cursive", "fantasy"];
    if GENERICS.contains(&name) {
        name.to_string()
    } else {
        let name = aliases.get(name).map(String::as_str).unwrap_or(name);
        format!("'{}'", crate::fonts::css_escape_family(name))
    }
}

#[cfg(all(test, feature = "presentation"))]
// Tests build a Config then tweak a field or two — the mutate-after-default form
// reads better here than struct-update with ..Default::default().
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use crate::config::{Config, FontSource, SymbolMap};

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
    fn hub_rendering_aliases_every_reference_to_a_loaded_family() {
        let mut cfg = Config::default();
        cfg.default_font = vec!["Text".into()];
        cfg.symbol_map = vec![SymbolMap {
            ranges: vec!["U+E0B0".into()],
            font: "Icons".into(),
        }];
        cfg.fonts.insert(
            "Text".into(),
            FontSource {
                weight_boost: Some(false),
                ..FontSource::default()
            },
        );
        let resolver = Resolver::build(&cfg).unwrap();
        let aliases = HashMap::from([
            ("Text".into(), "a".repeat(64)),
            ("Icons".into(), "b".repeat(64)),
        ]);

        let css = head_css_with_font_aliases("", &cfg, &aliases);
        assert!(
            css.contains(&format!("font-family:'{}',monospace", "a".repeat(64))),
            "{css}"
        );
        assert!(!css.contains("'Text'"), "{css}");

        let json = render_config_json_with_font_aliases(&cfg, &resolver, &aliases);
        assert!(json.contains(&format!(r#""fillFont":"'{}',monospace""#, "a".repeat(64))));
        assert!(json.contains(&format!(
            r#""'{}','{}',monospace""#,
            "b".repeat(64),
            "a".repeat(64)
        )));
        assert!(json.contains(&format!(r#""noBoost":["{}"]"#, "a".repeat(64))));
        assert!(!json.contains("Text") && !json.contains("Icons"), "{json}");
    }

    #[test]
    fn hub_embed_presentation_uses_only_hash_families_and_fixed_defaults() {
        let key = "a".repeat(64);
        let font_css = font_face_rule(&key, false, "fonts/", &key, "woff2");
        let css = hub_embed_head_css(&font_css, std::slice::from_ref(&key));
        assert!(css.contains(&format!("font-family:'{key}'")), "{css}");
        assert!(css.contains(&format!("#screen {{ font-family:'{key}',monospace;")));
        assert!(css.contains("font-size:14px"));
        assert!(css.contains("--lh:16.8px"));

        let cfg = hub_embed_render_config_json(std::slice::from_ref(&key));
        let parsed: serde_json::Value = serde_json::from_str(&cfg).unwrap();
        assert_eq!(parsed["fillFont"], format!("'{key}',monospace"));
        assert_eq!(parsed["fontPx"], 14);
        assert_eq!(parsed["lhPx"], 16.8);
        assert_eq!(parsed["sym"], serde_json::json!([]));
    }

    #[test]
    fn font_family_css_cannot_close_an_inline_style_element() {
        let mut cfg = Config::default();
        cfg.default_font = vec!["</style><script>alert(1)</script>".into()];
        let css = head_css("", &cfg);
        assert!(!css.contains("</style>"), "{css}");
        assert!(!css.contains("<script>"), "{css}");
        assert!(css.contains("\\3c /style>"), "{css}");

        let rule = font_face_rule(&cfg.default_font[0], false, "fonts/", "abc", "woff2");
        assert!(!rule.contains("</style>"), "{rule}");
        assert!(rule.contains("\\3c /style>"), "{rule}");
    }

    #[test]
    fn page_fills_template_tokens() {
        let tmpl = "<head>{{style}}</head><body>{{screen}}{{script}}</body>";
        let html = page(tmpl, "CSS", "<script>JS</script>");
        assert!(html.contains("<style>\nCSS</style>"), "{html}");
        // The screen ships empty — the renderer paints it from the first SSE full.
        assert!(html.contains("<div id=\"screen\"></div>"), "{html}");
        // The script block is injected raw (it carries its own <script> tags).
        assert!(html.contains("<script>JS</script>"), "{html}");
    }

    #[test]
    fn embed_assets_hold_their_contract() {
        // The embed template is a full viewer template (all three tokens),
        // carries the offline takeover, and dispatches sg-zoom so the canvas
        // re-rasterizes on fit changes.
        for tok in ["{{style}}", "{{screen}}", "{{script}}"] {
            assert!(EMBED_TEMPLATE.contains(tok), "embed template missing {tok}");
        }
        assert!(EMBED_TEMPLATE.contains("data-offline"), "offline takeover");
        assert!(EMBED_TEMPLATE.contains("sg-zoom"), "canvas re-raster hook");
        // The canvas mounts OUTSIDE the fit transform (WebKit resamples a
        // transformed canvas layer — soft glyphs); the id is the viewer
        // bootstrap's frozen hook.
        assert!(
            EMBED_TEMPLATE.contains(r#"id="sg-canvas-host""#),
            "canvas host outside the fit transform"
        );
        // embed.js is the STABLE public API: the one-liner's data-src, the
        // element name and its src attribute must never change, and both
        // forms must point at the ?embed page. currentScript = classic-script
        // self-insertion (modules would break it AND need CORS).
        assert!(EMBED_JS.contains("document.currentScript"));
        assert!(EMBED_JS.contains("dataset.src"));
        // Size defaults must stay zero-specificity (:where), so host
        // stylesheets override them without !important.
        assert!(EMBED_JS.contains(":where(iframe.shellglass-view)"));
        assert!(EMBED_JS.contains("customElements.define(\"shellglass-view\""));
        assert!(EMBED_JS.contains("getAttribute(\"src\")"));
        assert!(EMBED_JS.contains("searchParams.set(\"embed\""));
    }

    #[test]
    fn builtin_template_has_all_tokens() {
        for tok in ["{{style}}", "{{screen}}", "{{script}}"] {
            assert!(DEFAULT_TEMPLATE.contains(tok), "template missing {tok}");
        }
        // The canvas mounts OUTSIDE the fit transform (see the embed test).
        assert!(
            DEFAULT_TEMPLATE.contains(r#"id="sg-canvas-host""#),
            "canvas host outside the fit transform"
        );
        // The favicon link is RELATIVE (resolves under the page's directory —
        // `/` standalone, `/s/<slug>/` hub — so a subpath mount works), and
        // the asset is baked in.
        assert!(
            DEFAULT_TEMPLATE.contains(r#"href="favicon.svg""#),
            "template must link the favicon relatively"
        );
        assert!(
            FAVICON_SVG.starts_with("<svg") && FAVICON_SVG.contains("aria-label=\"shellglass\""),
            "baked favicon must be the shellglass SVG"
        );
    }

    #[test]
    fn font_face_css_references_content_addresses() {
        let fonts = vec![FontFile {
            family: "NF".into(),
            mime: "font/ttf",
            format: "truetype",
            bytes: vec![1, 2],
            bold: false,
        }];
        let key = fonts[0].key();
        assert_eq!(key, crate::proto::content_key("font/ttf", &[1, 2]));
        assert_eq!(key.len(), 64, "content address is sha256 hex");
        let css = font_face_css(&fonts, "fonts/");
        assert!(css.contains("font-family:'NF'"), "{css}");
        assert!(
            css.contains(&format!("src:url(\"fonts/{key}\") format('truetype')")),
            "{css}"
        );
    }
}

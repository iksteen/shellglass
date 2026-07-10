//! User configuration: rendering knobs, Kitty-style `symbol_map` font overrides,
//! and optional font-source hints (an explicit file path, or a lookup-name override).

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Base font stack: one family, or a list tried in order. The browser picks,
    /// per character, the first family in the stack that has a glyph for it — so
    /// listing a Nerd Font after your text font emulates Kitty's fallback without
    /// any `symbol_map` ranges. Accepts a string or an array of strings.
    #[serde(default = "default_font", deserialize_with = "string_or_vec")]
    pub default_font: Vec<String>,
    /// Terminal font size in px.
    #[serde(default = "default_font_size")]
    pub font_size_px: f32,
    /// Line-height multiple relative to `font_size_px`. Omit to derive it from
    /// the primary font's own metrics (ascender − descender + line gap, the way
    /// a terminal sizes its cells — see `fonts::metric_line_height`); the CLI
    /// resolves the omission at startup, so a `None` past setup means no font
    /// file was readable and the 1.2 fallback applies.
    #[serde(default)]
    pub line_height: Option<f32>,

    /// Codepoint-range → font overrides. First matching entry wins.
    #[serde(default)]
    pub symbol_map: Vec<SymbolMap>,
    /// Named font sources referenced by `default_font` / `symbol_map[].font`.
    #[serde(default)]
    pub fonts: HashMap<String, FontSource>,

    /// Path to a custom viewer HTML template. Tokens `{{style}}` / `{{screen}}` /
    /// `{{script}}` are filled in (see [`crate::render::DEFAULT_TEMPLATE`]). Omit
    /// for the built-in page (which carries its own in-page CRT-effect toggle).
    #[serde(default)]
    pub template: Option<PathBuf>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SymbolMap {
    /// e.g. `["U+E0A0-U+E0D4", "U+F000"]`.
    pub ranges: Vec<String>,
    /// Font family name (should exist as a key in `[fonts]`, or be system-installed).
    pub font: String,
}

/// Optional override for how a referenced family is located and served. Both
/// fields are optional: with neither, the family is looked up in the system font
/// database by its `[fonts]` key. `path` forces a specific file to serve; `system`
/// overrides the family name used for the database lookup when it differs from the
/// key.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct FontSource {
    pub path: Option<PathBuf>,
    pub system: Option<String>,
}

/// Family assumed installed system-wide for Nerd-Font symbol glyphs. It's part of
/// the built-in `default_font` fallback so icons render with no config; if it
/// isn't installed the browser just falls through (and it's exempt from the
/// unresolved-font warning, being our default rather than a user's typo).
pub(crate) const DEFAULT_SYMBOL_FONT: &str = "Symbols Nerd Font Mono";

fn default_font() -> Vec<String> {
    vec!["monospace".to_string(), DEFAULT_SYMBOL_FONT.to_string()]
}

/// Accept either `default_font = "Menlo"` or `default_font = ["Menlo", "NF"]`.
fn string_or_vec<'de, D>(d: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }
    Ok(match OneOrMany::deserialize(d)? {
        OneOrMany::One(s) => vec![s],
        OneOrMany::Many(v) => v,
    })
}
fn default_font_size() -> f32 {
    14.0
}
impl Default for Config {
    fn default() -> Self {
        Config {
            default_font: default_font(),
            font_size_px: default_font_size(),
            line_height: None,
            symbol_map: Vec::new(),
            fonts: HashMap::new(),
            template: None,
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Config> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let cfg: Config =
            toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))?;
        Ok(cfg)
    }

    /// Line height in px (`font_size_px * line_height`); 1.2 is the last-resort
    /// ratio when neither the config nor a font file provided one.
    pub fn line_height_px(&self) -> f32 {
        self.font_size_px * self.line_height.unwrap_or(1.2)
    }

    /// The viewer template: a configured file, else the built-in page.
    pub fn template_html(&self) -> Result<String> {
        match &self.template {
            Some(p) => std::fs::read_to_string(p)
                .with_context(|| format!("reading template {}", p.display())),
            None => Ok(crate::render::DEFAULT_TEMPLATE.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_font_accepts_string_or_list() {
        let one: Config = toml::from_str("default_font = \"Menlo\"").unwrap();
        assert_eq!(one.default_font, vec!["Menlo"]);
        let many: Config = toml::from_str("default_font = [\"Menlo\", \"NF\"]").unwrap();
        assert_eq!(many.default_font, vec!["Menlo", "NF"]);
        let absent: Config = toml::from_str("").unwrap();
        assert_eq!(absent.default_font, vec!["monospace", DEFAULT_SYMBOL_FONT]);
    }

    #[test]
    fn template_defaults_to_builtin_page() {
        // No config → the built-in page (which carries the CRT toggle itself).
        let def = Config::default().template_html().unwrap();
        assert_eq!(def, crate::render::DEFAULT_TEMPLATE);

        // An explicit template file wins (points at a missing path → err).
        let file: Config = toml::from_str("template = \"/no/such/file\"").unwrap();
        assert!(file.template_html().is_err());
    }
}

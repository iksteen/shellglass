//! User configuration: rendering knobs, Kitty-style `symbol_map` font overrides,
//! and font sources (embedded file or installed system family).

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Base monospace family used when no `symbol_map` entry matches.
    #[serde(default = "default_font")]
    pub default_font: String,
    /// Terminal font size in px.
    #[serde(default = "default_font_size")]
    pub font_size_px: f32,
    /// Line-height multiple relative to `font_size_px`.
    #[serde(default = "default_line_height")]
    pub line_height: f32,

    /// Codepoint-range → font overrides. First matching entry wins.
    #[serde(default)]
    pub symbol_map: Vec<SymbolMap>,
    /// Named font sources referenced by `default_font` / `symbol_map[].font`.
    #[serde(default)]
    pub fonts: HashMap<String, FontSource>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SymbolMap {
    /// e.g. `["U+E0A0-U+E0D4", "U+F000"]`.
    pub ranges: Vec<String>,
    /// Font family name (should exist as a key in `[fonts]`, or be system-installed).
    pub font: String,
}

/// A font is either embedded from a file (`path`) or referenced by an installed
/// family name (`system`). If neither is set, the family name is used verbatim.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct FontSource {
    pub path: Option<PathBuf>,
    /// Installed family to reference instead of embedding (no `@font-face`).
    #[allow(dead_code)]
    pub system: Option<String>,
}

fn default_font() -> String {
    "monospace".to_string()
}
fn default_font_size() -> f32 {
    14.0
}
fn default_line_height() -> f32 {
    1.2
}

impl Default for Config {
    fn default() -> Self {
        Config {
            default_font: default_font(),
            font_size_px: default_font_size(),
            line_height: default_line_height(),
            symbol_map: Vec::new(),
            fonts: HashMap::new(),
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

    /// Line height in px (`font_size_px * line_height`).
    pub fn line_height_px(&self) -> f32 {
        self.font_size_px * self.line_height
    }
}

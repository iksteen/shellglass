//! Kitty-style `symbol_map`: resolve a character's codepoint to a font family,
//! and build the embedded `@font-face` CSS for configured font sources.

use crate::config::Config;
use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use std::ops::RangeInclusive;
use std::path::Path;

/// Compiled font-override table: codepoint ranges paired with a family name.
/// First matching range wins, mirroring Kitty's `symbol_map` semantics.
pub struct Resolver {
    entries: Vec<(RangeInclusive<u32>, String)>,
}

impl Resolver {
    pub fn build(config: &Config) -> Result<Resolver> {
        let mut entries = Vec::new();
        for sm in &config.symbol_map {
            for spec in &sm.ranges {
                let range = parse_range(spec)
                    .with_context(|| format!("invalid symbol_map range {spec:?}"))?;
                entries.push((range, sm.font.clone()));
            }
        }
        Ok(Resolver { entries })
    }

    /// Family name overriding the default for `ch`, if any matches.
    pub fn font_for(&self, ch: char) -> Option<&str> {
        let cp = ch as u32;
        self.entries
            .iter()
            .find(|(r, _)| r.contains(&cp))
            .map(|(_, f)| f.as_str())
    }
}

/// Parse `"U+E0A0-U+E0D4"` or a single `"U+F000"` into an inclusive range.
fn parse_range(spec: &str) -> Result<RangeInclusive<u32>> {
    let parse_cp = |s: &str| -> Result<u32> {
        let s = s.trim();
        let hex = s
            .strip_prefix("U+")
            .or_else(|| s.strip_prefix("u+"))
            .or_else(|| s.strip_prefix("0x"))
            .unwrap_or(s);
        u32::from_str_radix(hex, 16).with_context(|| format!("bad codepoint {s:?}"))
    };
    match spec.split_once('-') {
        Some((a, b)) => {
            let (lo, hi) = (parse_cp(a)?, parse_cp(b)?);
            if lo > hi {
                bail!("range start > end in {spec:?}");
            }
            Ok(lo..=hi)
        }
        None => {
            let cp = parse_cp(spec)?;
            Ok(cp..=cp)
        }
    }
}

/// Build `@font-face` blocks for every font source that points at a file.
/// System-referenced fonts (or bare family names) produce no `@font-face`.
///
/// An unavailable embedded font (missing/unreadable file, bad extension) is a
/// soft failure: warn and skip its `@font-face` so the family name simply falls
/// through to an installed copy or the next font in the stack, rather than
/// aborting the whole page. Configs can therefore point at optional local files.
pub fn font_face_css(config: &Config) -> String {
    let mut css = String::new();
    for (name, src) in &config.fonts {
        let Some(path) = &src.path else { continue };
        match font_face(name, path) {
            Ok(face) => css.push_str(&face),
            Err(e) => eprintln!("tmuxsnitch: skipping font {name:?}: {e:#}"),
        }
    }
    css
}

fn font_face(name: &str, path: &Path) -> Result<String> {
    let (mime, format) = font_format(path)?;
    let bytes =
        std::fs::read(path).with_context(|| format!("reading font file {}", path.display()))?;
    let b64 = B64.encode(&bytes);
    Ok(format!(
        "@font-face {{ font-family: '{}'; src: url(data:{};base64,{}) format('{}'); }}\n",
        css_escape_family(name),
        mime,
        b64,
        format,
    ))
}

fn font_format(path: &Path) -> Result<(&'static str, &'static str)> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "woff2" => Ok(("font/woff2", "woff2")),
        "woff" => Ok(("font/woff", "woff")),
        "ttf" => Ok(("font/ttf", "truetype")),
        "otf" => Ok(("font/otf", "opentype")),
        other => bail!("unsupported font extension {other:?} for {}", path.display()),
    }
}

/// Family names go inside a single-quoted CSS string; neutralize quotes/backslashes.
fn css_escape_family(name: &str) -> String {
    name.replace('\\', "\\\\").replace('\'', "\\'")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::FontSource;

    #[test]
    fn missing_embedded_font_soft_fails() {
        // A path that doesn't exist must not abort — it's skipped, other fonts kept.
        let mut cfg = Config::default();
        cfg.fonts.insert(
            "Ghost".into(),
            FontSource { path: Some("/no/such/font.ttf".into()), system: None },
        );
        cfg.fonts.insert(
            "Named".into(),
            FontSource { path: None, system: Some("Named".into()) },
        );
        let css = font_face_css(&cfg);
        assert!(!css.contains("Ghost"), "missing font should be skipped: {css}");
    }
}

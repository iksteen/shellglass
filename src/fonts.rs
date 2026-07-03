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

/// CSS generic families that always resolve without a source.
const GENERICS: [&str; 5] = ["monospace", "serif", "sans-serif", "cursive", "fantasy"];

/// Families referenced by `default_font` / `symbol_map`, deduped in first-seen
/// order, minus CSS generics (which need no file).
fn referenced_families(config: &Config) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    config
        .default_font
        .iter()
        .chain(config.symbol_map.iter().map(|s| &s.font))
        .filter(|f| !GENERICS.contains(&f.as_str()))
        .filter(|f| seen.insert((*f).clone()))
        .cloned()
        .collect()
}

/// A font file located on this host, to be served to viewers so they render the
/// glyphs without a local install. `family` is the CSS name it's referenced by.
pub struct FontFile {
    pub family: String,
    pub mime: &'static str,
    pub format: &'static str,
    pub bytes: Vec<u8>,
}

/// Locate and read every referenced family's font file: an explicit `[fonts].path`
/// wins, otherwise ask fontconfig (`fc-match`) for the installed file. Missing or
/// unreadable fonts are soft failures (warn + skip) so the page still renders with
/// browser fallback. Run once at startup by the process that owns the fonts
/// (standalone or push client); the hub just serves what the client uploads.
pub fn collect_fonts(config: &Config) -> Vec<FontFile> {
    let mut out = Vec::new();
    for family in referenced_families(config) {
        let entry = config.fonts.get(&family);
        let path = entry.and_then(|s| s.path.clone()).or_else(|| {
            let name = entry.and_then(|s| s.system.as_deref()).unwrap_or(&family);
            locate_font(name)
        });
        let Some(path) = path else {
            // The built-in default symbol font is expected to be absent on some
            // hosts; don't nag about it. Anything the user configured, do warn.
            if family != crate::config::DEFAULT_SYMBOL_FONT {
                eprintln!(
                    "tmuxsnitch: font {family:?} not found on this host — viewers without it \
                     installed will see fallback glyphs"
                );
            }
            continue;
        };
        match load_font(&family, &path) {
            Ok(f) => out.push(f),
            Err(e) => eprintln!("tmuxsnitch: skipping font {family:?}: {e:#}"),
        }
    }
    out
}

fn load_font(family: &str, path: &Path) -> Result<FontFile> {
    let (mime, format) = font_format(path)?;
    let bytes =
        std::fs::read(path).with_context(|| format!("reading font file {}", path.display()))?;
    Ok(FontFile { family: family.to_string(), mime, format, bytes })
}

/// Ask fontconfig for the file backing `name`. fc-match always returns *some*
/// font, so accept the result only if its family actually matches `name` — a
/// mismatch means the font isn't installed and fontconfig substituted a default.
/// ponytail: shells out to fc-match (the system font DB on Linux); absent or no
/// match → None and the caller falls back to browser rendering.
fn locate_font(name: &str) -> Option<std::path::PathBuf> {
    let (families, path) = fc_query(name)?;
    // %{family} is a comma-separated alias list; accept only an exact
    // (case-insensitive) hit — otherwise fontconfig substituted a default, i.e.
    // the requested font isn't installed and we mustn't serve the wrong file.
    families
        .split(',')
        .any(|a| a.trim().eq_ignore_ascii_case(name))
        .then_some(path)
}

/// Raw `fc-match`: the `%{family}` alias list and file path fontconfig returns for
/// `name` (always *some* font). Returns None only if fc-match is absent/fails.
fn fc_query(name: &str) -> Option<(String, std::path::PathBuf)> {
    let out = std::process::Command::new("fc-match")
        .arg("-f")
        .arg("%{family}|%{file}")
        .arg(name)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let (families, file) = stdout.split_once('|')?;
    Some((families.to_string(), std::path::PathBuf::from(file.trim())))
}

/// Expand CSS-generic entries in `default_font` (`monospace`, `serif`, …) to the
/// host's concrete font family so viewers render the same face rather than their
/// own browser default — for a mirror, the host's monospace *is* the content. The
/// generic stays as the stack's last-resort fallback (appended by `font_stack`);
/// downstream (`collect_fonts`, `font_stack`) then treats the concrete name like
/// any other family. Left untouched if fontconfig can't resolve it.
pub fn resolve_generics(config: &mut Config) {
    for fam in &mut config.default_font {
        if !GENERICS.contains(&fam.as_str()) {
            continue;
        }
        // Take fontconfig's substitution as-is (that's what a generic means); use
        // its primary alias as the concrete family name to reference and serve.
        if let Some((families, _)) = fc_query(fam) {
            if let Some(concrete) = families.split(',').next().map(str::trim) {
                if !concrete.is_empty() {
                    *fam = concrete.to_string();
                }
            }
        }
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

/// Encode located fonts for upload to the hub. The key is the font's index and
/// MUST match the URL index [`crate::render::font_face_css`] bakes into the CSS.
pub fn font_assets(fonts: &[FontFile]) -> Vec<crate::proto::FontAsset> {
    fonts
        .iter()
        .enumerate()
        .map(|(i, f)| crate::proto::FontAsset {
            key: i.to_string(),
            mime: f.mime.to_string(),
            b64: B64.encode(&f.bytes),
        })
        .collect()
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
pub(crate) fn css_escape_family(name: &str) -> String {
    name.replace('\\', "\\\\").replace('\'', "\\'")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{FontSource, SymbolMap};

    #[test]
    fn referenced_families_dedupe_and_drop_generics() {
        let mut cfg = Config::default();
        cfg.default_font = vec!["monospace".into(), "Menlo".into()];
        cfg.symbol_map = vec![
            SymbolMap { ranges: vec!["U+E0B0".into()], font: "Menlo".into() }, // dup
            SymbolMap { ranges: vec!["U+F000".into()], font: "NF".into() },
        ];
        // monospace dropped (generic); Menlo appears once despite two references.
        assert_eq!(referenced_families(&cfg), vec!["Menlo".to_string(), "NF".to_string()]);
    }

    #[test]
    fn resolve_generics_pins_monospace_to_a_concrete_font() {
        let mut cfg = Config::default(); // ["monospace", "Symbols Nerd Font Mono"]
        resolve_generics(&mut cfg);
        // With fontconfig present the generic becomes a concrete (non-generic)
        // family; without it, it's left as-is. Either way it must not vanish, and
        // the non-monospace entry is untouched.
        let first = &cfg.default_font[0];
        assert!(!first.is_empty());
        if first != "monospace" {
            assert!(!GENERICS.contains(&first.as_str()), "resolved to another generic: {first}");
        }
        assert_eq!(cfg.default_font[1], "Symbols Nerd Font Mono");
    }

    #[test]
    fn collect_skips_unlocatable_and_missing_path() {
        // Neither a family fontconfig can't match nor a bad explicit path yields a
        // FontFile — collection soft-fails instead of aborting. (No dependence on
        // any particular font being installed: we only assert these are absent.)
        let mut cfg = Config::default();
        cfg.default_font = vec!["monospace".into(), "Definitely Not A Font 9271".into()];
        cfg.fonts.insert(
            "Ghost".into(),
            FontSource { path: Some("/no/such/font.ttf".into()), system: None },
        );
        cfg.symbol_map = vec![SymbolMap { ranges: vec!["U+F000".into()], font: "Ghost".into() }];
        let fonts = collect_fonts(&cfg);
        assert!(
            fonts.iter().all(|f| f.family != "Definitely Not A Font 9271" && f.family != "Ghost"),
            "unlocatable/missing fonts must not be collected"
        );
    }
}

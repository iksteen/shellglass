//! Kitty-style `symbol_map` codepoint→family resolution, plus locating the font
//! files referenced by the config so they can be served to viewers.
//!
//! Font *file* lookup uses [`fontdb`] — a pure-Rust, cross-platform system font
//! database, no subprocess. It hands back the file *and the face index*, so a
//! single face can be extracted from a `.ttc` collection and served as a standalone
//! web font (browsers can't select a face from a `.ttc`). The only platform tool
//! consulted is `fc-match`, purely to learn the OS's default family for a CSS
//! generic (`monospace`, …); it's optional and absent on macOS/Windows.

// Everything except [`CACHE_CONTROL_IMMUTABLE`] and [`FontFile`] is the discovery
// machinery — mirror-side only (roadmap item 13): the hub never locates fonts,
// it just re-serves what clients push.
#[cfg(feature = "presentation")]
use crate::config::Config;
#[cfg(feature = "presentation")]
use anyhow::{Context, Result, bail};
#[cfg(feature = "presentation")]
use base64::Engine;
#[cfg(feature = "presentation")]
use base64::engine::general_purpose::STANDARD as B64;
#[cfg(feature = "presentation")]
use fontdb::{Database, Family, Query, Stretch, Style, Weight};
#[cfg(feature = "presentation")]
use std::ops::RangeInclusive;
#[cfg(feature = "presentation")]
use std::path::Path;
#[cfg(feature = "presentation")]
use std::sync::OnceLock;

/// Compiled font-override table: codepoint ranges paired with a family name.
/// First matching range wins, mirroring Kitty's `symbol_map` semantics.
#[cfg(feature = "presentation")]
pub struct Resolver {
    entries: Vec<(RangeInclusive<u32>, String)>,
}

#[cfg(feature = "presentation")]
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

    /// The compiled `(range, family)` overrides, so the browser renderer can be
    /// handed the symbol table and resolve fonts client-side (see
    /// [`crate::render::render_config_json`]).
    pub fn entries(&self) -> &[(RangeInclusive<u32>, String)] {
        &self.entries
    }
}

/// CSS generic families that always resolve without a source.
#[cfg(feature = "presentation")]
const GENERICS: [&str; 5] = ["monospace", "serif", "sans-serif", "cursive", "fantasy"];

/// Families referenced by `default_font` / `symbol_map`, deduped in first-seen
/// order, minus CSS generics (which need no file).
#[cfg(feature = "presentation")]
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

/// `Cache-Control` for served fonts. A font's bytes are stable for the life of a
/// Immutable: font URLs are content addresses (`fonts/<content_key>`), so the
/// bytes behind a URL can never change — safe to cache forever.
pub const CACHE_CONTROL_IMMUTABLE: &str = "public, max-age=31536000, immutable";

/// A font file located on this host, to be served to viewers so they render the
/// glyphs without a local install. `family` is the CSS name it's referenced by.
pub struct FontFile {
    pub family: String,
    pub mime: &'static str,
    pub format: &'static str,
    pub bytes: Vec<u8>,
    /// True for a family's bold face — emitted as a `font-weight:bold` `@font-face`
    /// so bold cells render at real weight and monospace width. Without it the
    /// browser fakes bold, which is wider than the cell and gets clipped.
    pub bold: bool,
}

impl FontFile {
    /// The font's content address ([`crate::proto::content_key`]): the key its
    /// `@font-face` URL references and the hub stores/serves it under.
    #[must_use]
    pub fn key(&self) -> String {
        crate::proto::content_key(self.mime, &self.bytes)
    }
}

/// The process-wide system font database, built once (scanning font dirs is not
/// cheap). Shared by generic resolution and per-family location.
#[cfg(feature = "presentation")]
fn system_db() -> &'static Database {
    static DB: OnceLock<Database> = OnceLock::new();
    DB.get_or_init(|| {
        let mut db = Database::new();
        db.load_system_fonts();
        db
    })
}

/// Locate and read every referenced family's font file: an explicit `[fonts].path`
/// wins, otherwise `fontdb` finds the installed file. `.ttc` faces are extracted to
/// standalone web fonts. Missing/unreadable fonts are soft failures (warn + skip)
/// so the page still renders with browser fallback. Run once at startup by the
/// process that owns the fonts (standalone or push client); the hub validates the
/// structured metadata, generates CSS, and serves the uploaded bytes.
#[cfg(feature = "presentation")]
pub fn collect_fonts(config: &Config) -> Vec<FontFile> {
    let db = system_db();
    let mut out = Vec::new();
    for family in referenced_families(config) {
        let entry = config.fonts.get(&family);
        if let Some(path) = entry.and_then(|s| s.path.as_deref()) {
            // Explicit path: one file, one (regular) face — no bold variant to find.
            match load_font_file(&family, path) {
                Ok(f) => out.push(f),
                Err(e) => eprintln!("shellglass: skipping font {family:?}: {e:#}"),
            }
            continue;
        }
        // `system` overrides the name to look up when it differs from the key.
        let name = entry.and_then(|s| s.system.as_deref()).unwrap_or(&family);
        let Some((normal_id, bytes, mime, format)) = fontdb_locate(db, name, Weight::NORMAL) else {
            // The built-in default symbol font is expected to be absent on some
            // hosts; don't nag about it. Anything the user configured, do warn.
            if family != crate::config::DEFAULT_SYMBOL_FONT {
                eprintln!(
                    "shellglass: font {family:?} not found on this host — viewers without it \
                     installed will see fallback glyphs"
                );
            }
            continue;
        };
        out.push(FontFile {
            family: family.clone(), // reference it by the config key, not the lookup name
            mime,
            format,
            bytes,
            bold: false,
        });
        // Ship the family's bold face too when it's a distinct file, so bold cells
        // render at real weight and monospace width. Best-effort — a family with no
        // separate bold resolves back to `normal_id`; skip it (browser faux-bold).
        if let Some((bold_id, bytes, mime, format)) = fontdb_locate(db, name, Weight::BOLD)
            && bold_id != normal_id
        {
            out.push(FontFile {
                family: family.clone(),
                mime,
                format,
                bytes,
                bold: true,
            });
        }
    }
    out
}

/// Read an explicit font file: WOFF/WOFF2 are already single-face web fonts and
/// pass through; ttf/otf/ttc are normalized to a standalone sfnt (face 0).
#[cfg(feature = "presentation")]
fn load_font_file(family: &str, path: &Path) -> Result<FontFile> {
    let raw = std::fs::read(path).with_context(|| format!("reading font {}", path.display()))?;
    let (bytes, mime, format) = match raw.get(0..4) {
        Some(b"wOFF") => (raw, "font/woff", "woff"),
        Some(b"wOF2") => (raw, "font/woff2", "woff2"),
        _ => {
            let (bytes, format) =
                sfnt_face(&raw, 0).with_context(|| format!("parsing font {}", path.display()))?;
            (bytes, mime_for(format), format)
        }
    };
    Ok(FontFile {
        family: family.to_string(),
        mime,
        format,
        bytes,
        bold: false,
    })
}

/// Find the installed file for family `name` at `weight` via fontdb and return its
/// face id + servable bytes. fontdb matches by family and doesn't substitute, but
/// double-check the match to be safe. The id lets the caller tell a real bold face
/// from a `weight`-request that fell back to the regular one.
#[cfg(feature = "presentation")]
fn fontdb_locate(
    db: &Database,
    name: &str,
    weight: Weight,
) -> Option<(fontdb::ID, Vec<u8>, &'static str, &'static str)> {
    let id = db.query(&Query {
        families: &[Family::Name(name)],
        weight,
        stretch: Stretch::Normal,
        style: Style::Normal,
    })?;
    let info = db.face(id)?;
    if !info
        .families
        .iter()
        .any(|(f, _)| f.eq_ignore_ascii_case(name))
    {
        return None;
    }
    let (bytes, format) = db.with_face_data(id, sfnt_face)??;
    Some((id, bytes, mime_for(format), format))
}

/// Expand CSS-generic entries in `default_font` (`monospace`, `serif`, …) to the
/// host's concrete font family so viewers render the same face rather than their
/// own browser default — for a mirror, the host's monospace *is* the content. The
/// generic stays as the stack's last-resort fallback (appended by `font_stack`);
/// downstream (`collect_fonts`, `font_stack`) then treats the concrete name like
/// any other family. Left untouched if nothing suitable is installed.
#[cfg(feature = "presentation")]
pub fn resolve_generics(config: &mut Config) {
    let db = system_db();
    for fam in &mut config.default_font {
        if !GENERICS.contains(&fam.as_str()) {
            continue;
        }
        if let Some(concrete) = concrete_generic(db, fam) {
            *fam = concrete;
        }
    }
}

/// A concrete installed family backing a CSS generic. Prefer the OS's configured
/// default (fontconfig, where present); otherwise — macOS, Windows, or no
/// fontconfig — fall back to a cross-platform candidate list. fontdb serves the
/// file either way, and the generic stays as the ultimate CSS fallback regardless.
#[cfg(feature = "presentation")]
fn concrete_generic(db: &Database, generic: &str) -> Option<String> {
    if let Some(fam) = fontconfig_default(generic)
        && family_installed(db, &fam)
    {
        return Some(fam);
    }
    let candidates: &[&str] = match generic {
        "monospace" => &[
            "Menlo",
            "DejaVu Sans Mono",
            "Noto Sans Mono",
            "Liberation Mono",
            "Consolas",
            "Cascadia Mono",
            "Courier New",
        ],
        "serif" => &[
            "Times New Roman",
            "Times",
            "DejaVu Serif",
            "Noto Serif",
            "Liberation Serif",
        ],
        "sans-serif" => &[
            "Helvetica",
            "Arial",
            "DejaVu Sans",
            "Noto Sans",
            "Liberation Sans",
        ],
        "cursive" => &["Apple Chancery", "Comic Sans MS"],
        "fantasy" => &["Papyrus", "Impact"],
        _ => return None,
    };
    candidates
        .iter()
        .find(|c| family_installed(db, c))
        .map(std::string::ToString::to_string)
}

/// kitty's cell height, as a multiple of the em: the primary font's hhea line
/// advance — ascender − descender + line gap — read from the file we serve
/// (kitty freetype.c `calc_cell_height`: FreeType's `metrics.height`, the same
/// three hhea fields). This is how a terminal sizes its cell FROM the font, so
/// ink fits the box by construction instead of by a blind ratio.
#[cfg(feature = "presentation")]
pub fn metric_line_height(fonts: &[FontFile]) -> Option<f32> {
    let first = fonts.iter().find(|f| !f.bold)?;
    let face = ttf_parser::Face::parse(&first.bytes, 0).ok()?;
    let upem = f32::from(face.units_per_em());
    if upem <= 0.0 {
        return None;
    }
    let advance =
        f32::from(face.ascender()) - f32::from(face.descender()) + f32::from(face.line_gap());
    (advance > 0.0).then_some(advance / upem)
}

/// The concrete family fontconfig resolves a CSS generic to — i.e. the OS/user
/// configured default. `None` if `fc-match` isn't present (macOS/Windows). Used
/// only for the generic→family hint; fontdb still locates and serves the file.
#[cfg(feature = "presentation")]
fn fontconfig_default(generic: &str) -> Option<String> {
    let out = std::process::Command::new("fc-match")
        .arg("-f")
        .arg("%{family}")
        .arg(generic)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let fam = stdout.split(',').next()?.trim(); // %{family} may be an alias list
    (!fam.is_empty()).then(|| fam.to_string())
}

#[cfg(feature = "presentation")]
fn family_installed(db: &Database, name: &str) -> bool {
    db.faces().any(|f| {
        f.families
            .iter()
            .any(|(fam, _)| fam.eq_ignore_ascii_case(name))
    })
}

/// Extract face `index` from `data` (a TTF/OTF, or one face of a TTC) as a
/// standalone sfnt, so it can be served as a single web font. Rebuilds the offset
/// table + table directory pointing at 4-aligned copies of the face's tables and
/// recomputes `head.checkSumAdjustment`. Returns the bytes and CSS `format`
/// keyword; `None` on malformed input (all indexing is bounds-checked).
#[cfg(feature = "presentation")]
fn sfnt_face(data: &[u8], index: u32) -> Option<(Vec<u8>, &'static str)> {
    // A TTC starts with 'ttcf' then per-face offsets to each face's offset table.
    let dir = if data.get(0..4)? == b"ttcf" {
        let num = read_u32(data, 8)? as usize;
        if index as usize >= num {
            return None;
        }
        read_u32(data, 12 + 4 * index as usize)? as usize
    } else {
        0
    };

    let sfnt_ver = data.get(dir..dir + 4)?.to_vec();
    let num_tables = read_u16(data, dir + 4)? as usize;
    if num_tables == 0 {
        return None;
    }

    // Collect each table's (tag, checksum, body) from the face's directory.
    let mut tables = Vec::with_capacity(num_tables);
    for i in 0..num_tables {
        let rec = dir + 12 + i * 16;
        let tag = data.get(rec..rec + 4)?.to_vec();
        let checksum = read_u32(data, rec + 4)?;
        let off = read_u32(data, rec + 8)? as usize;
        let len = read_u32(data, rec + 12)? as usize;
        let body = data.get(off..off + len)?.to_vec();
        tables.push((tag, checksum, body));
    }

    // Write a fresh single-face sfnt: offset table, records, 4-aligned bodies.
    let mut out = Vec::new();
    out.extend_from_slice(&sfnt_ver);
    let (sr, es, rs) = sfnt_search_params(num_tables);
    out.extend_from_slice(&(num_tables as u16).to_be_bytes());
    out.extend_from_slice(&sr.to_be_bytes());
    out.extend_from_slice(&es.to_be_bytes());
    out.extend_from_slice(&rs.to_be_bytes());
    let rec_start = out.len();
    out.resize(rec_start + num_tables * 16, 0);
    let mut head_off = None;
    for (i, (tag, checksum, body)) in tables.iter().enumerate() {
        while out.len() % 4 != 0 {
            out.push(0);
        }
        let off = out.len() as u32;
        if tag.as_slice() == b"head" {
            head_off = Some(off as usize);
        }
        out.extend_from_slice(body);
        let rec = rec_start + i * 16;
        out[rec..rec + 4].copy_from_slice(tag);
        out[rec + 4..rec + 8].copy_from_slice(&checksum.to_be_bytes());
        out[rec + 8..rec + 12].copy_from_slice(&off.to_be_bytes());
        out[rec + 12..rec + 16].copy_from_slice(&(body.len() as u32).to_be_bytes());
    }

    // head.checkSumAdjustment = 0xB1B0AFBA − checksum(whole file, field zeroed).
    if let Some(h) = head_off
        && out.len() >= h + 12
    {
        out[h + 8..h + 12].copy_from_slice(&[0; 4]);
        let adj = 0xB1B0_AFBAu32.wrapping_sub(sfnt_checksum(&out));
        out[h + 8..h + 12].copy_from_slice(&adj.to_be_bytes());
    }

    let format = if sfnt_ver == b"OTTO" {
        "opentype"
    } else {
        "truetype"
    };
    Some((out, format))
}

#[cfg(feature = "presentation")]
fn read_u16(d: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(d.get(at..at + 2)?.try_into().ok()?))
}
#[cfg(feature = "presentation")]
fn read_u32(d: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(d.get(at..at + 4)?.try_into().ok()?))
}

/// Sum of the data as big-endian u32 words, zero-padding a trailing partial word.
#[cfg(feature = "presentation")]
fn sfnt_checksum(d: &[u8]) -> u32 {
    d.chunks(4).fold(0u32, |acc, c| {
        let mut w = [0u8; 4];
        w[..c.len()].copy_from_slice(c);
        acc.wrapping_add(u32::from_be_bytes(w))
    })
}

/// sfnt offset-table `searchRange`, `entrySelector`, `rangeShift` for `n` tables.
#[cfg(feature = "presentation")]
fn sfnt_search_params(n: usize) -> (u16, u16, u16) {
    let mut es = 0u16;
    let mut p = 1usize;
    while p * 2 <= n {
        p *= 2;
        es += 1;
    }
    let sr = (p * 16) as u16;
    let rs = (n * 16) as u16 - sr;
    (sr, es, rs)
}

#[cfg(feature = "presentation")]
fn mime_for(format: &str) -> &'static str {
    match format {
        "opentype" => "font/otf",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        _ => "font/ttf",
    }
}

/// Parse `"U+E0A0-U+E0D4"` or a single `"U+F000"` into an inclusive range.
#[cfg(feature = "presentation")]
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
    if let Some((a, b)) = spec.split_once('-') {
        let (lo, hi) = (parse_cp(a)?, parse_cp(b)?);
        if lo > hi {
            bail!("range start > end in {spec:?}");
        }
        Ok(lo..=hi)
    } else {
        let cp = parse_cp(spec)?;
        Ok(cp..=cp)
    }
}

/// Hub-mode font upload plus the mapping from local config family names to the
/// regular face's content hash. The aliases are used only while building the
/// pushed base CSS/render config; standalone serve keeps the configured names.
#[cfg(feature = "presentation")]
pub struct HubFontBundle {
    pub assets: Vec<crate::proto::FontAsset>,
    pub aliases: std::collections::HashMap<String, String>,
}

/// Encode located fonts for upload to the hub. Each family is named by the
/// content key of its first regular face; bold faces reference that same key.
/// A bold-only family is omitted because it cannot establish a family identity.
#[cfg(feature = "presentation")]
pub fn hub_font_bundle(fonts: &[FontFile]) -> HubFontBundle {
    let mut aliases = std::collections::HashMap::new();
    for f in fonts.iter().filter(|f| !f.bold) {
        aliases.entry(f.family.clone()).or_insert_with(|| f.key());
    }

    let assets = fonts
        .iter()
        .filter_map(|f| {
            let family_key = aliases.get(&f.family)?;
            // `collect_fonts` produces one regular face per family. Keep this
            // invariant explicit if a future/custom producer supplies duplicates.
            if !f.bold && f.key() != *family_key {
                return None;
            }
            Some(crate::proto::FontAsset {
                family_key: family_key.clone(),
                mime: f.mime.to_string(),
                b64: B64.encode(&f.bytes),
                bold: f.bold,
            })
        })
        .collect();
    HubFontBundle { assets, aliases }
}

/// Family names go inside a single-quoted CSS string which may itself be embedded
/// in an HTML `<style>` element. Escape CSS syntax plus `<`: the HTML raw-text
/// parser recognizes `</style>` even inside CSS quotes, so leaving `<` literal
/// would turn an operator-controlled family name into a script-tag breakout.
pub(crate) fn css_escape_family(name: &str) -> String {
    use std::fmt::Write as _;

    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '<' => out.push_str("\\3c "),
            c if c.is_control() => {
                let _ = write!(out, "\\{:x} ", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out
}

#[cfg(all(test, feature = "presentation"))]
// Tests build a Config then tweak a field or two — the mutate-after-default form
// reads better here than struct-update with ..Default::default().
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use crate::config::{FontSource, SymbolMap};

    #[test]
    fn referenced_families_dedupe_and_drop_generics() {
        let mut cfg = Config::default();
        cfg.default_font = vec!["monospace".into(), "Menlo".into()];
        cfg.symbol_map = vec![
            SymbolMap {
                ranges: vec!["U+E0B0".into()],
                font: "Menlo".into(),
            }, // dup
            SymbolMap {
                ranges: vec!["U+F000".into()],
                font: "NF".into(),
            },
        ];
        // monospace dropped (generic); Menlo appears once despite two references.
        assert_eq!(
            referenced_families(&cfg),
            vec!["Menlo".to_string(), "NF".to_string()]
        );
    }

    #[test]
    fn hub_font_bundle_keys_family_by_its_regular_face() {
        let fonts = vec![
            FontFile {
                family: "Family".into(),
                mime: "font/woff2",
                format: "woff2",
                bytes: vec![1, 2, 3],
                bold: false,
            },
            FontFile {
                family: "Family".into(),
                mime: "font/woff2",
                format: "woff2",
                bytes: vec![4, 5, 6],
                bold: true,
            },
            FontFile {
                family: "Bold only".into(),
                mime: "font/woff2",
                format: "woff2",
                bytes: vec![7, 8, 9],
                bold: true,
            },
        ];
        let bundle = hub_font_bundle(&fonts);
        let family_key = fonts[0].key();
        assert_eq!(bundle.aliases.get("Family"), Some(&family_key));
        assert!(!bundle.aliases.contains_key("Bold only"));
        assert_eq!(bundle.assets.len(), 2);
        assert!(
            bundle
                .assets
                .iter()
                .all(|asset| asset.family_key == family_key)
        );
        assert!(!bundle.assets[0].bold);
        assert!(bundle.assets[1].bold);
        assert_eq!(B64.decode(&bundle.assets[1].b64).unwrap(), [4, 5, 6]);
    }

    #[test]
    fn extracts_single_face_from_ttc() {
        // Build a 1-face TTC with a 'head' (≥12 bytes) and a 'cmap', extract face 0,
        // and confirm both tables survive and the head checksum is made valid.
        let head = vec![0u8; 54];
        let cmap = vec![1u8, 2, 3, 4, 5];
        let tables: [(&[u8; 4], &[u8]); 2] = [(b"head", &head), (b"cmap", &cmap)];
        let ttc = build_ttc(b"\x00\x01\x00\x00", &tables);

        let (out, format) = sfnt_face(&ttc, 0).expect("extract face 0");
        assert_eq!(format, "truetype");
        assert!(sfnt_face(&ttc, 1).is_none(), "only one face exists");

        let n = u16::from_be_bytes([out[4], out[5]]) as usize;
        assert_eq!(n, 2);
        let table = |tag: &[u8; 4]| -> Vec<u8> {
            for i in 0..n {
                let rec = 12 + i * 16;
                if &out[rec..rec + 4] == tag {
                    let off = read_u32(&out, rec + 8).unwrap() as usize;
                    let len = read_u32(&out, rec + 12).unwrap() as usize;
                    return out[off..off + len].to_vec();
                }
            }
            panic!("table {tag:?} missing from extracted sfnt");
        };
        assert_eq!(table(b"cmap"), cmap);
        assert_eq!(table(b"head").len(), 54);
        // With checkSumAdjustment applied, the whole file sums to the magic constant.
        assert_eq!(
            sfnt_checksum(&out),
            0xB1B0_AFBA,
            "head checkSumAdjustment wrong"
        );
    }

    /// Lay out a single-face TTC with table offsets absolute from file start.
    fn build_ttc(version: &[u8; 4], tables: &[(&[u8; 4], &[u8])]) -> Vec<u8> {
        let n = tables.len();
        let dir_off = 16usize; // 'ttcf' + version + numFonts + one face offset
        let mut sfnt = Vec::new();
        sfnt.extend_from_slice(version);
        sfnt.extend_from_slice(&(n as u16).to_be_bytes());
        sfnt.extend_from_slice(&[0u8; 6]); // search params (ignored by reader)
        let rec_start = sfnt.len();
        sfnt.resize(rec_start + n * 16, 0);
        for (i, (tag, body)) in tables.iter().enumerate() {
            while !(dir_off + sfnt.len()).is_multiple_of(4) {
                sfnt.push(0);
            }
            let off = (dir_off + sfnt.len()) as u32;
            sfnt.extend_from_slice(body);
            let rec = rec_start + i * 16;
            sfnt[rec..rec + 4].copy_from_slice(*tag);
            sfnt[rec + 8..rec + 12].copy_from_slice(&off.to_be_bytes());
            sfnt[rec + 12..rec + 16].copy_from_slice(&(body.len() as u32).to_be_bytes());
        }
        let mut out = Vec::new();
        out.extend_from_slice(b"ttcf");
        out.extend_from_slice(&0x0001_0000u32.to_be_bytes());
        out.extend_from_slice(&1u32.to_be_bytes());
        out.extend_from_slice(&(dir_off as u32).to_be_bytes());
        out.extend_from_slice(&sfnt);
        out
    }

    #[test]
    fn resolve_generics_pins_monospace_to_a_concrete_font() {
        let mut cfg = Config::default(); // ["monospace", "Symbols Nerd Font Mono"]
        resolve_generics(&mut cfg);
        // With a monospace font installed the generic becomes a concrete (non-
        // generic) family; without one, it's left as-is. Either way it must not
        // vanish, and the non-monospace entry is untouched.
        let first = &cfg.default_font[0];
        assert!(!first.is_empty());
        if first != "monospace" {
            assert!(
                !GENERICS.contains(&first.as_str()),
                "resolved to another generic: {first}"
            );
        }
        assert_eq!(cfg.default_font[1], "Symbols Nerd Font Mono");
    }

    #[test]
    fn collect_skips_unlocatable_and_missing_path() {
        // Neither a family fontdb can't match nor a bad explicit path yields a
        // FontFile — collection soft-fails instead of aborting. (No dependence on
        // any particular font being installed: we only assert these are absent.)
        let mut cfg = Config::default();
        cfg.default_font = vec!["monospace".into(), "Definitely Not A Font 9271".into()];
        cfg.fonts.insert(
            "Ghost".into(),
            FontSource {
                path: Some("/no/such/font.ttf".into()),
                system: None,
                ..Default::default()
            },
        );
        cfg.symbol_map = vec![SymbolMap {
            ranges: vec!["U+F000".into()],
            font: "Ghost".into(),
        }];
        let fonts = collect_fonts(&cfg);
        assert!(
            fonts
                .iter()
                .all(|f| f.family != "Definitely Not A Font 9271" && f.family != "Ghost"),
            "unlocatable/missing fonts must not be collected"
        );
    }

    #[test]
    fn metric_line_height_reads_real_font_metrics() {
        assert_eq!(metric_line_height(&[]), None, "no fonts, no ratio");
        // Any real installed monospace: hhea advance sits in a sane band —
        // tighter than 0.8em means a broken face, taller than 2em means we
        // misread the table. Skip quietly on fontless CI hosts.
        let mut cfg = Config::default();
        crate::fonts::resolve_generics(&mut cfg);
        let fonts = collect_fonts(&cfg);
        if fonts.is_empty() {
            return;
        }
        let ratio = metric_line_height(&fonts).expect("a served font file parses");
        assert!(
            (0.8..=2.0).contains(&ratio),
            "hhea line advance out of any plausible range: {ratio}"
        );
    }
}

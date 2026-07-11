//! Parser-agnostic intermediate representation. Nothing here depends on `vt100`,
//! so the input/parse layer can be swapped without touching the renderer.
//!
//! These are in-memory types only — the wire format (compact columnar cells,
//! rectangle deltas) lives entirely in [`crate::diff`]. Only [`Color`] carries
//! serde impls, because the wire's cell styles embed it (compact: `Default` is
//! omitted by the container, `Idx(i)` is the bare number, `Rgb` is `[r,g,b]`).

use serde::de::{self, SeqAccess, Visitor};
use serde::ser::SerializeSeq;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Terminal color, mirroring the three cases every VT model produces. Serialized
/// compactly: `Default` is omitted by the cell's `skip_serializing_if`, `Idx(i)`
/// is the bare number `i`, `Rgb(r,g,b)` is `[r,g,b]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Color {
    #[default]
    Default,
    Idx(u8),
    Rgb(u8, u8, u8),
}

impl Serialize for Color {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match *self {
            // Default never actually serializes (cells skip it), but map it to null
            // so a stray serialization round-trips rather than colliding with Idx(0).
            Color::Default => s.serialize_none(),
            Color::Idx(i) => s.serialize_u8(i),
            Color::Rgb(r, g, b) => {
                let mut seq = s.serialize_seq(Some(3))?;
                seq.serialize_element(&r)?;
                seq.serialize_element(&g)?;
                seq.serialize_element(&b)?;
                seq.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for Color {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Color, D::Error> {
        struct ColorVisitor;
        impl<'de> Visitor<'de> for ColorVisitor {
            type Value = Color;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("null, a 0-255 palette index, or an [r,g,b] array")
            }
            fn visit_none<E>(self) -> Result<Color, E> {
                Ok(Color::Default)
            }
            fn visit_unit<E>(self) -> Result<Color, E> {
                Ok(Color::Default)
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Color, E> {
                u8::try_from(v)
                    .map(Color::Idx)
                    .map_err(|_| E::custom("palette index out of range"))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Color, A::Error> {
                let r = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::missing_field("r"))?;
                let g = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::missing_field("g"))?;
                let b = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::missing_field("b"))?;
                Ok(Color::Rgb(r, g, b))
            }
        }
        d.deserialize_any(ColorVisitor)
    }
}

/// One rendered cell. Wide (double-width) cells carry their glyph and are marked
/// `wide`; their trailing continuation column is dropped during parsing.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StyledCell {
    /// Grapheme content. Empty string means a blank cell (rendered as a space).
    pub text: String,
    pub fg: Color,
    pub bg: Color,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    /// Underline style, kitty's SGR `4:n` numbering: 0 none, 1 single,
    /// 2 double, 3 curly, 4 dotted, 5 dashed.
    pub underline: u8,
    /// Strikethrough (SGR 9/29).
    pub strike: bool,
    /// Conceal (SGR 8/28): the glyph is hidden, the text stays in the buffer.
    pub concealed: bool,
    /// Blink (SGR 5/6, 25 off). kitty renders it; so do the viewers.
    pub blink: bool,
    /// Underline color (SGR 58/59); `Default` = follow the text color.
    pub ulcolor: Color,
    pub inverse: bool,
    /// OSC 8 hyperlink id, resolved through [`Grid::links`]. Rides the wire
    /// as style key `a`.
    pub link: Option<u32>,
    /// Occupies two terminal columns.
    pub wide: bool,
}

pub(crate) fn is_default_color(c: &Color) -> bool {
    *c == Color::Default
}
#[allow(clippy::trivially_copy_pass_by_ref)] // signature required by serde's skip_serializing_if
pub(crate) fn is_false(b: &bool) -> bool {
    !*b
}

/// An inline image (iTerm2/kitty/sixel) placed at a terminal cell. The wire
/// carries the PLACEMENT only — position, size, and the image's content
/// address ([`crate::proto::content_key`]) under the `k` key; the browser
/// fetches the bytes over HTTP at the page-relative `images/<k>` (immutable,
/// so reconnecting viewers hit their cache instead of re-downloading —
/// in-frame base64 could never be cached and multiplied by every viewer).
/// The bytes themselves live in [`Grid::image_data`] on the producing side
/// and travel client→hub as blob messages, never inside frames.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct ImagePlacement {
    /// Top-left cell of the image. May be negative when the image has partially
    /// scrolled off the top: the viewer clips it above the screen edge.
    #[serde(rename = "r")]
    pub row: i16,
    #[serde(rename = "c")]
    pub col: u16,
    /// Display size in cells, if the app specified one (else the browser uses the
    /// image's natural pixel size).
    #[serde(rename = "w", default, skip_serializing_if = "Option::is_none")]
    pub cols: Option<u16>,
    #[serde(rename = "h", default, skip_serializing_if = "Option::is_none")]
    pub rows: Option<u16>,
    /// Content address of the image bytes ([`crate::proto::content_key`]).
    #[serde(rename = "k")]
    pub hash: String,
}

/// An image payload, keyed by its content address in [`Grid::image_data`].
/// `Bytes` because a blob is cloned into every frame its placement is visible
/// in — the multi-MB payload must ride along by refcount, not by copy.
#[derive(Debug, Clone, PartialEq)]
pub struct ImageBlob {
    pub mime: String,
    /// The raw image file bytes (what [`crate::proto::content_key`] hashes;
    /// base64 only exists transiently on the blob wire message).
    pub bytes: bytes::Bytes,
}

/// The terminal screen as cells. `rows[r]` holds the visible cells of row `r`, with
/// wide continuation columns already removed (so a row may be shorter than `cols`).
#[derive(Debug, Clone, PartialEq)]
pub struct Grid {
    /// Nominal column count (the screen width in cells).
    pub cols: u16,
    pub rows: Vec<Vec<StyledCell>>,
    /// Cursor (row, col) if visible.
    pub cursor: Option<(u16, u16)>,
    /// DECSCUSR cursor style, raw 0-6: 0 default, 1/2 blinking/steady block,
    /// 3/4 underline, 5/6 bar. Rides the wire as the optional `q` key.
    pub cursor_style: u8,
    /// OSC 10/11 default fg/bg overrides; `Color::Default` = the configured
    /// default. Ride the wire as the optional full-frame `e` key (a change
    /// forces a full frame, like images).
    pub default_colors: (Color, Color),
    /// Window title (OSC 0/2), empty if never set. Rides the wire as the
    /// optional `t` key — full frames and the Diff shape only, NEVER the
    /// flattened `c` form: an old viewer spreads unknown `c`-envelope keys
    /// into every cell, where `t` would overwrite cell *text*.
    pub title: String,
    /// OSC 8 hyperlink table for the ids referenced by on-screen cells,
    /// id → URI. Rides the wire as the optional `y` key: the whole table on
    /// full frames, new entries only on the Diff shape (a diff introducing a
    /// new id forces that shape).
    pub links: std::collections::BTreeMap<u32, String>,
    /// Inline images currently placed on the screen (empty for the common case).
    pub images: Vec<ImagePlacement>,
    /// The payloads behind `images`, keyed by content address — POPULATED ONLY
    /// on the producing side (PTY backend): the standalone server serves them
    /// at `images/<key>` and the push client uploads them as blob messages.
    /// Never on the wire; a Grid decoded from wire messages (the hub's applied
    /// matrix) has it empty — the hub keeps blobs in its per-session store.
    pub image_data: std::collections::HashMap<String, ImageBlob>,
}

/// What a backend publishes on the frame channel: a live screen snapshot, or an
/// error banner to show in place of the screen. The client streams its wire-encoded
/// deltas to the hub; the standalone server and hub both keep the current `Frame`
/// in a [`crate::diff::Live`].
#[derive(Debug, Clone, PartialEq)]
pub enum Frame {
    Screen(Grid),
    Banner(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_serde_is_compact_and_roundtrips() {
        // Idx rides as a bare number, Rgb as an array.
        assert_eq!(serde_json::to_string(&Color::Idx(9)).unwrap(), "9");
        assert_eq!(
            serde_json::to_string(&Color::Rgb(1, 2, 3)).unwrap(),
            "[1,2,3]"
        );
        // Deserialization accepts all three forms (null = Default).
        assert_eq!(serde_json::from_str::<Color>("9").unwrap(), Color::Idx(9));
        assert_eq!(
            serde_json::from_str::<Color>("[1,2,3]").unwrap(),
            Color::Rgb(1, 2, 3)
        );
        assert_eq!(
            serde_json::from_str::<Color>("null").unwrap(),
            Color::Default
        );
    }
}

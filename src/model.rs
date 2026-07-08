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
    pub underline: bool,
    pub inverse: bool,
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

/// An inline image (iTerm2/kitty) placed at a terminal cell, forwarded to the
/// browser as a `data:` URL overlay. Serializes compactly for the full-frame wire
/// message under the `i` key (see [`crate::diff`]); the browser decodes the base64.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    #[serde(rename = "m")]
    pub mime: String,
    /// The image file, base64.
    #[serde(rename = "d")]
    pub data: String,
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
    /// Inline images currently placed on the screen (empty for the common case).
    pub images: Vec<ImagePlacement>,
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

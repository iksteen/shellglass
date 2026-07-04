//! Parser-agnostic intermediate representation. Nothing here depends on `vt100`,
//! so the input/parse layer can be swapped without touching the renderer.
//!
//! These types are also the diff/stream wire format: a backend pushes a [`Frame`]
//! (a `Grid` snapshot, or an error `Banner`) which the client (`client.rs`) may
//! serialize to a hub, and [`crate::diff`] turns successive `Grid`s into the compact
//! rectangle deltas the browser renderer applies. Cell serialization is deliberately
//! compact (short keys, defaults omitted) — a blank cell is `{}`.

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
/// `wide`; their trailing continuation column is dropped during parsing. Serde keys
/// are single letters and every default is omitted, so a blank cell is `{}` and a
/// plain letter is `{"t":"a"}`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct StyledCell {
    /// Grapheme content. Empty string means a blank cell (rendered as a space).
    #[serde(rename = "t", default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    #[serde(rename = "f", default, skip_serializing_if = "is_default_color")]
    pub fg: Color,
    #[serde(rename = "g", default, skip_serializing_if = "is_default_color")]
    pub bg: Color,
    #[serde(rename = "b", default, skip_serializing_if = "is_false")]
    pub bold: bool,
    #[serde(rename = "d", default, skip_serializing_if = "is_false")]
    pub dim: bool,
    #[serde(rename = "i", default, skip_serializing_if = "is_false")]
    pub italic: bool,
    #[serde(rename = "u", default, skip_serializing_if = "is_false")]
    pub underline: bool,
    #[serde(rename = "n", default, skip_serializing_if = "is_false")]
    pub inverse: bool,
    /// Occupies two terminal columns.
    #[serde(rename = "w", default, skip_serializing_if = "is_false")]
    pub wide: bool,
}

pub(crate) fn is_default_color(c: &Color) -> bool {
    *c == Color::Default
}
#[allow(clippy::trivially_copy_pass_by_ref)] // signature required by serde's skip_serializing_if
pub(crate) fn is_false(b: &bool) -> bool {
    !*b
}

/// The terminal screen as cells. `rows[r]` holds the visible cells of row `r`, with
/// wide continuation columns already removed (so a row may be shorter than `cols`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Grid {
    /// Nominal column count (the screen width in cells).
    pub cols: u16,
    pub rows: Vec<Vec<StyledCell>>,
    /// Cursor (row, col) if visible.
    pub cursor: Option<(u16, u16)>,
}

/// What a backend publishes on the frame channel: a live screen snapshot, or an
/// error banner to show in place of the screen. The client serializes this to the
/// hub; the standalone server and hub both feed it to [`crate::diff::Live`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Frame {
    Screen(Grid),
    Banner(String),
}

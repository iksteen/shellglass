use unicode_width::UnicodeWidthChar as _;

// chosen to make the size of the cell struct 32 bytes upstream (52 with the
// shellglass image tag and underline-color + hyperlink attrs)
const CONTENT_BYTES: usize = 22;

const IS_WIDE: u8 = 0b1000_0000;
const IS_WIDE_CONTINUATION: u8 = 0b0100_0000;
const LEN_BITS: u8 = 0b0001_1111;

/// shellglass: one cell's share of an inline-image placement.
///
/// See [`Screen::place_image`](crate::Screen::place_image). The offsets locate
/// this cell within the image, so any surviving cell reconstructs the
/// placement's top-left exactly — scrolling, line insertion/deletion, and
/// erasure need no extra tracking.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ImageCell {
    id: std::num::NonZeroU32,
    row_off: u16,
    col_off: u16,
}

impl ImageCell {
    pub(crate) fn new(
        id: std::num::NonZeroU32,
        row_off: u16,
        col_off: u16,
    ) -> Self {
        Self {
            id,
            row_off,
            col_off,
        }
    }

    /// The placement id passed to
    /// [`Screen::place_image`](crate::Screen::place_image).
    #[must_use]
    pub fn id(self) -> std::num::NonZeroU32 {
        self.id
    }

    /// Rows below the image's top edge.
    #[must_use]
    pub fn row_off(self) -> u16 {
        self.row_off
    }

    /// Columns right of the image's left edge.
    #[must_use]
    pub fn col_off(self) -> u16 {
        self.col_off
    }
}

/// Represents a single terminal cell.
#[derive(Clone, Debug, Eq)]
pub struct Cell {
    contents: [u8; CONTENT_BYTES],
    len: u8,
    attrs: crate::attrs::Attrs,
    // shellglass: inline-image tag; dies with the cell's contents (set/clear),
    // which is exactly a cell-based sixel terminal's erase semantics.
    image: Option<ImageCell>,
}
const _: () = assert!(std::mem::size_of::<Cell>() == 52);

impl PartialEq<Self> for Cell {
    fn eq(&self, other: &Self) -> bool {
        if self.len != other.len {
            return false;
        }
        if self.attrs != other.attrs {
            return false;
        }
        let len = self.len();
        self.contents[..len] == other.contents[..len]
    }
}

impl Cell {
    pub(crate) fn new() -> Self {
        Self {
            contents: Default::default(),
            len: 0,
            attrs: crate::attrs::Attrs::default(),
            image: None,
        }
    }

    fn len(&self) -> usize {
        usize::from(self.len & LEN_BITS)
    }

    pub(crate) fn set(&mut self, c: char, a: crate::attrs::Attrs) {
        self.len = 0;
        self.image = None; // shellglass: overwriting text erases the image here
        self.append_char(0, c);
        // strings in this context should always be an arbitrary character
        // followed by zero or more zero-width characters, so we should only
        // have to look at the first character
        self.set_wide(c.width().unwrap_or(1) > 1);
        self.attrs = a;
    }

    pub(crate) fn append(&mut self, c: char) {
        let len = self.len();
        if len >= CONTENT_BYTES - 4 {
            return;
        }
        if len == 0 {
            self.contents[0] = b' ';
            self.len += 1;
        }

        // we already checked that we have space for another codepoint
        self.append_char(self.len(), c);
    }

    // Writes bytes representing c at start
    // Requires caller to verify start <= CODEPOINTS_IN_CELL * 4
    fn append_char(&mut self, start: usize, c: char) {
        c.encode_utf8(&mut self.contents[start..]);
        self.len += u8::try_from(c.len_utf8()).unwrap();
    }

    pub(crate) fn clear(&mut self, attrs: crate::attrs::Attrs) {
        self.len = 0;
        self.attrs = attrs;
        // shellglass: an erased cell keeps drawing attrs (bg) but must never
        // be a clickable link, and erasing the cell erases the image here.
        self.attrs.link = None;
        self.image = None;
    }

    /// shellglass: this cell's share of an inline-image placement, if it is
    /// covered by one (see [`Screen::place_image`](crate::Screen::place_image)).
    #[must_use]
    pub fn image_cell(&self) -> Option<ImageCell> {
        self.image
    }

    // shellglass: stamp (or clear) the image tag without touching the cell's
    // text or attributes — the terminal draws images *over* cells.
    pub(crate) fn set_image(&mut self, image: Option<ImageCell>) {
        self.image = image;
    }

    /// Returns the text contents of the cell.
    ///
    /// Can include multiple unicode characters if combining characters are
    /// used, but will contain at most one character with a non-zero character
    /// width.
    // Since contents has been constructed by appending chars encoded as UTF-8 it will be valid UTF-8
    #[allow(clippy::missing_panics_doc)]
    #[must_use]
    pub fn contents(&self) -> &str {
        std::str::from_utf8(&self.contents[..self.len()]).unwrap()
    }

    /// Returns whether the cell contains any text data.
    #[must_use]
    pub fn has_contents(&self) -> bool {
        self.len() > 0
    }

    /// Returns whether the text data in the cell represents a wide character.
    #[must_use]
    pub fn is_wide(&self) -> bool {
        self.len & IS_WIDE != 0
    }

    /// Returns whether the cell contains the second half of a wide character
    /// (in other words, whether the previous cell in the row contains a wide
    /// character)
    #[must_use]
    pub fn is_wide_continuation(&self) -> bool {
        self.len & IS_WIDE_CONTINUATION != 0
    }

    fn set_wide(&mut self, wide: bool) {
        if wide {
            self.len |= IS_WIDE;
        } else {
            self.len &= !IS_WIDE;
        }
    }

    pub(crate) fn set_wide_continuation(&mut self, wide: bool) {
        if wide {
            self.len |= IS_WIDE_CONTINUATION;
        } else {
            self.len &= !IS_WIDE_CONTINUATION;
        }
    }

    pub(crate) fn attrs(&self) -> &crate::attrs::Attrs {
        &self.attrs
    }

    /// Returns the foreground color of the cell.
    #[must_use]
    pub fn fgcolor(&self) -> crate::Color {
        self.attrs.fgcolor
    }

    /// Returns the background color of the cell.
    #[must_use]
    pub fn bgcolor(&self) -> crate::Color {
        self.attrs.bgcolor
    }

    /// Returns whether the cell should be rendered with the bold text
    /// attribute.
    #[must_use]
    pub fn bold(&self) -> bool {
        self.attrs.bold()
    }

    /// Returns whether the cell should be rendered with the dim text
    /// attribute.
    #[must_use]
    pub fn dim(&self) -> bool {
        self.attrs.dim()
    }

    /// Returns whether the cell should be rendered with the italic text
    /// attribute.
    #[must_use]
    pub fn italic(&self) -> bool {
        self.attrs.italic()
    }

    /// Returns whether the cell should be rendered with the underlined text
    /// attribute.
    #[must_use]
    pub fn underline(&self) -> bool {
        self.attrs.underline()
    }

    /// shellglass: the underline style — 0 none, 1 single, 2 double, 3 curly,
    /// 4 dotted, 5 dashed (SGR `4:n` / `21`, kitty's numbering).
    #[must_use]
    pub fn underline_style(&self) -> u8 {
        self.attrs.underline_style()
    }

    /// shellglass: whether the cell should be rendered struck through
    /// (SGR 9/29).
    #[must_use]
    pub fn strikethrough(&self) -> bool {
        self.attrs.strikethrough()
    }

    /// shellglass: the underline color (SGR 58/59); `Color::Default` means
    /// the underline follows the text color.
    #[must_use]
    pub fn ulcolor(&self) -> crate::Color {
        self.attrs.ulcolor
    }

    /// shellglass: the OSC 8 hyperlink id covering this cell, resolved via
    /// [`Screen::link_uri`](crate::Screen::link_uri).
    #[must_use]
    pub fn link(&self) -> Option<std::num::NonZeroU32> {
        self.attrs.link
    }

    /// Returns whether the cell should be rendered with the inverse text
    /// attribute.
    #[must_use]
    pub fn inverse(&self) -> bool {
        self.attrs.inverse()
    }
}

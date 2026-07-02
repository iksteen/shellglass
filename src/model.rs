//! Parser-agnostic intermediate representation. Nothing here depends on `vt100`,
//! so the input/parse layer can be swapped (e.g. tmux control mode) without
//! touching the renderer.

/// Terminal color, mirroring the three cases every VT model produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Color {
    Default,
    Idx(u8),
    Rgb(u8, u8, u8),
}

/// One rendered cell. Wide (double-width) cells carry their glyph and are marked
/// `wide`; their trailing continuation column is dropped during parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
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

/// A pane's cell grid. `rows[r]` holds the visible cells of row `r`, with wide
/// continuation columns already removed (so a row may be shorter than `cols`).
#[derive(Debug, Clone)]
pub struct Grid {
    /// Nominal column count (kept for the eventual live/diff path).
    #[allow(dead_code)]
    pub cols: u16,
    pub rows: Vec<Vec<StyledCell>>,
    /// Cursor (row, col) if visible in this pane.
    pub cursor: Option<(u16, u16)>,
}

/// Placement of a pane within its window, in cell units (from tmux).
#[derive(Debug, Clone)]
pub struct PaneGeom {
    pub id: String,
    pub left: u16,
    pub top: u16,
    pub width: u16,
    pub height: u16,
    pub active: bool,
}

#[derive(Debug, Clone)]
pub struct Pane {
    pub geom: PaneGeom,
    pub grid: Grid,
}

/// A full window snapshot: overall size plus every pane.
#[derive(Debug, Clone)]
pub struct Window {
    pub width: u16,
    pub height: u16,
    pub panes: Vec<Pane>,
}

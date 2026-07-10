use crate::term::BufWrite as _;

/// Represents a foreground or background color for cells.
#[derive(Eq, PartialEq, Debug, Copy, Clone, Default)]
pub enum Color {
    /// The default terminal color.
    #[default]
    Default,

    /// An indexed terminal color.
    Idx(u8),

    /// An RGB terminal color. The parameters are (red, green, blue).
    Rgb(u8, u8, u8),
}

const TEXT_MODE_INTENSITY: u8 = 0b0000_0011;
const TEXT_MODE_BOLD: u8 = 0b0000_0001;
const TEXT_MODE_DIM: u8 = 0b0000_0010;
const TEXT_MODE_ITALIC: u8 = 0b0000_0100;
// shellglass: upstream's underline bit became strikethrough; underline moved
// to the 3-bit style field below (0 = no underline).
const TEXT_MODE_STRIKETHROUGH: u8 = 0b0000_1000;
const TEXT_MODE_INVERSE: u8 = 0b0001_0000;
// shellglass: underline style (SGR 4:n / 21 / 24), kitty's numbering — 0 none,
// 1 single, 2 double, 3 curly, 4 dotted, 5 dashed.
const TEXT_MODE_UNDERLINE_SHIFT: u8 = 5;
const TEXT_MODE_UNDERLINE: u8 = 0b1110_0000;

#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
pub struct Attrs {
    pub fgcolor: Color,
    pub bgcolor: Color,
    // shellglass: underline color (SGR 58/59); Default = follow the text color.
    pub ulcolor: Color,
    pub mode: u8,
}

impl Attrs {
    pub fn bold(&self) -> bool {
        self.mode & TEXT_MODE_BOLD != 0
    }

    pub fn dim(&self) -> bool {
        self.mode & TEXT_MODE_DIM != 0
    }

    fn intensity(&self) -> u8 {
        self.mode & TEXT_MODE_INTENSITY
    }

    pub fn set_bold(&mut self) {
        self.mode &= !TEXT_MODE_INTENSITY;
        self.mode |= TEXT_MODE_BOLD;
    }

    pub fn set_dim(&mut self) {
        self.mode &= !TEXT_MODE_INTENSITY;
        self.mode |= TEXT_MODE_DIM;
    }

    pub fn set_normal_intensity(&mut self) {
        self.mode &= !TEXT_MODE_INTENSITY;
    }

    pub fn italic(&self) -> bool {
        self.mode & TEXT_MODE_ITALIC != 0
    }

    pub fn set_italic(&mut self, italic: bool) {
        if italic {
            self.mode |= TEXT_MODE_ITALIC;
        } else {
            self.mode &= !TEXT_MODE_ITALIC;
        }
    }

    pub fn underline(&self) -> bool {
        self.underline_style() != 0
    }

    pub fn set_underline(&mut self, underline: bool) {
        self.set_underline_style(u8::from(underline));
    }

    // shellglass: 0 none, 1 single, 2 double, 3 curly, 4 dotted, 5 dashed.
    pub fn underline_style(&self) -> u8 {
        (self.mode & TEXT_MODE_UNDERLINE) >> TEXT_MODE_UNDERLINE_SHIFT
    }

    pub fn set_underline_style(&mut self, style: u8) {
        debug_assert!(style <= 5);
        self.mode &= !TEXT_MODE_UNDERLINE;
        self.mode |=
            (style << TEXT_MODE_UNDERLINE_SHIFT) & TEXT_MODE_UNDERLINE;
    }

    // shellglass: strikethrough (SGR 9/29).
    pub fn strikethrough(&self) -> bool {
        self.mode & TEXT_MODE_STRIKETHROUGH != 0
    }

    pub fn set_strikethrough(&mut self, strikethrough: bool) {
        if strikethrough {
            self.mode |= TEXT_MODE_STRIKETHROUGH;
        } else {
            self.mode &= !TEXT_MODE_STRIKETHROUGH;
        }
    }

    pub fn inverse(&self) -> bool {
        self.mode & TEXT_MODE_INVERSE != 0
    }

    pub fn set_inverse(&mut self, inverse: bool) {
        if inverse {
            self.mode |= TEXT_MODE_INVERSE;
        } else {
            self.mode &= !TEXT_MODE_INVERSE;
        }
    }

    pub fn write_escape_code_diff(
        &self,
        contents: &mut Vec<u8>,
        other: &Self,
    ) {
        if self != other && self == &Self::default() {
            crate::term::ClearAttrs.write_buf(contents);
            return;
        }

        let attrs = crate::term::Attrs::default();

        let attrs = if self.fgcolor == other.fgcolor {
            attrs
        } else {
            attrs.fgcolor(self.fgcolor)
        };
        let attrs = if self.bgcolor == other.bgcolor {
            attrs
        } else {
            attrs.bgcolor(self.bgcolor)
        };
        let attrs = if self.intensity() == other.intensity() {
            attrs
        } else {
            attrs.intensity(match self.intensity() {
                0 => crate::term::Intensity::Normal,
                TEXT_MODE_BOLD => crate::term::Intensity::Bold,
                TEXT_MODE_DIM => crate::term::Intensity::Dim,
                _ => unreachable!(),
            })
        };
        let attrs = if self.italic() == other.italic() {
            attrs
        } else {
            attrs.italic(self.italic())
        };
        // shellglass: underline rides its style (upstream's bool is gone).
        let attrs = if self.underline_style() == other.underline_style() {
            attrs
        } else {
            attrs.underline_style(self.underline_style())
        };
        let attrs = if self.strikethrough() == other.strikethrough() {
            attrs
        } else {
            attrs.strikethrough(self.strikethrough())
        };
        let attrs = if self.ulcolor == other.ulcolor {
            attrs
        } else {
            attrs.ulcolor(self.ulcolor)
        };
        let attrs = if self.inverse() == other.inverse() {
            attrs
        } else {
            attrs.inverse(self.inverse())
        };

        attrs.write_buf(contents);
    }
}

mod helpers;

#[test]
fn colors() {
    helpers::fixture("colors");
}

#[test]
fn attrs() {
    helpers::fixture("attrs");
}

#[test]
fn attributes_formatted() {
    let mut parser = vt100::Parser::default();
    assert_eq!(parser.screen().attributes_formatted(), b"\x1b[m");
    parser.process(b"\x1b[32mfoo\x1b[41mbar\x1b[33mbaz");
    assert_eq!(parser.screen().attributes_formatted(), b"\x1b[m\x1b[33;41m");
    parser.process(b"\x1b[1m\x1b[39m");
    assert_eq!(parser.screen().attributes_formatted(), b"\x1b[m\x1b[41;1m");
    parser.process(b"\x1b[m");
    assert_eq!(parser.screen().attributes_formatted(), b"\x1b[m");
}

// shellglass: modern SGR — underline styles (4:n / 21 / 24), strikethrough
// (9/29), underline color (58/59 in all three shapes). helix and neovim emit
// 4:3 undercurl for diagnostics; before this, the underline was lost entirely.
#[test]
fn modern_sgr() {
    let mut vt = vt100::Parser::default();
    let cell = |vt: &vt100::Parser, col: u16| {
        let c = vt.screen().cell(0, col).unwrap();
        (c.underline_style(), c.strikethrough(), c.ulcolor())
    };

    vt.process(b"\x1b[4ma\x1b[4:3mb\x1b[21mc\x1b[9md\x1b[24;29me");
    assert_eq!(cell(&vt, 0), (1, false, vt100::Color::Default));
    assert_eq!(cell(&vt, 1), (3, false, vt100::Color::Default));
    assert!(vt.screen().cell(0, 1).unwrap().underline());
    assert_eq!(cell(&vt, 2), (2, false, vt100::Color::Default));
    assert_eq!(cell(&vt, 3), (2, true, vt100::Color::Default));
    assert_eq!(cell(&vt, 4), (0, false, vt100::Color::Default));

    // 4:0 clears the underline (not the strikethrough); SGR 0 clears everything.
    vt.process(b"\x1b[9;4:5m\x1b[4:0mf\x1b[mg");
    assert_eq!(cell(&vt, 5), (0, true, vt100::Color::Default));
    assert_eq!(cell(&vt, 6), (0, false, vt100::Color::Default));

    // Underline color: all three shapes, the kitty colon form with an empty
    // colorspace id, and the 59 reset.
    vt.process(b"\x1b[4:3;58;5;196mh\x1b[58;2;1;2;3mi");
    vt.process(b"\x1b[58:2::9:8:7mj\x1b[59mk");
    assert_eq!(cell(&vt, 7), (3, false, vt100::Color::Idx(196)));
    assert_eq!(cell(&vt, 8), (3, false, vt100::Color::Rgb(1, 2, 3)));
    assert_eq!(cell(&vt, 9), (3, false, vt100::Color::Rgb(9, 8, 7)));
    assert_eq!(cell(&vt, 10), (3, false, vt100::Color::Default));
}

// shellglass: the repaint path (contents_formatted) re-emits the modern SGR
// state — feeding its output to a fresh parser reproduces the same attrs.
#[test]
fn modern_sgr_formatted_roundtrip() {
    let mut vt = vt100::Parser::default();
    vt.process(b"\x1b[4:3;9;58;2;10;20;30mx\x1b[21;59my");
    let mut vt2 = vt100::Parser::default();
    vt2.process(&vt.screen().contents_formatted());
    for col in 0..2 {
        let (a, b) = (
            vt.screen().cell(0, col).unwrap(),
            vt2.screen().cell(0, col).unwrap(),
        );
        assert_eq!(a.underline_style(), b.underline_style(), "col {col}");
        assert_eq!(a.strikethrough(), b.strikethrough(), "col {col}");
        assert_eq!(a.ulcolor(), b.ulcolor(), "col {col}");
    }
}

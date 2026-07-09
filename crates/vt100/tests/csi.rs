mod helpers;

#[test]
fn absolute_movement() {
    helpers::fixture("absolute_movement");
}

#[test]
fn row_clamp() {
    let mut vt = vt100::Parser::default();
    assert_eq!(vt.screen().cursor_position(), (0, 0));
    vt.process(b"\x1b[15d");
    assert_eq!(vt.screen().cursor_position(), (14, 0));
    vt.process(b"\x1b[150d");
    assert_eq!(vt.screen().cursor_position(), (23, 0));
}

#[test]
fn relative_movement() {
    helpers::fixture("relative_movement");
}

#[test]
fn ed() {
    helpers::fixture("ed");
}

#[test]
fn el() {
    helpers::fixture("el");
}

#[test]
fn ich_dch_ech() {
    helpers::fixture("ich_dch_ech");
}

#[test]
fn il_dl() {
    helpers::fixture("il_dl");
}

#[test]
fn scroll() {
    helpers::fixture("scroll");
}

#[test]
fn xtwinops() {
    struct Callbacks;
    impl vt100::Callbacks for Callbacks {
        fn resize(
            &mut self,
            screen: &mut vt100::Screen,
            (rows, cols): (u16, u16),
        ) {
            screen.set_size(rows, cols);
        }
    }

    let mut vt = vt100::Parser::new_with_callbacks(24, 80, 0, Callbacks);
    assert_eq!(vt.screen().size(), (24, 80));
    vt.process(b"\x1b[8;24;80t");
    assert_eq!(vt.screen().size(), (24, 80));
    vt.process(b"\x1b[8t");
    assert_eq!(vt.screen().size(), (24, 80));
    vt.process(b"\x1b[8;80;24t");
    assert_eq!(vt.screen().size(), (80, 24));
    vt.process(b"\x1b[8;24t");
    assert_eq!(vt.screen().size(), (24, 24));

    let mut vt = vt100::Parser::new_with_callbacks(24, 80, 0, Callbacks);
    assert_eq!(vt.screen().size(), (24, 80));
    vt.process(b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    assert_eq!(
        vt.screen().rows(0, 80).next().unwrap(),
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    );
    assert_eq!(vt.screen().rows(0, 80).nth(1).unwrap(), "aaaaaaaaaa");
    vt.process(
        b"\x1b[H\x1b[8;24;15tbbbbbbbbbbbbbbbbbbbb\x1b[8;24;80tcccccccccccccccccccc",
    );
    assert_eq!(vt.screen().rows(0, 80).next().unwrap(), "bbbbbbbbbbbbbbb");
    assert_eq!(
        vt.screen().rows(0, 80).nth(1).unwrap(),
        "bbbbbcccccccccccccccccccc"
    );
}

// shellglass: SCOSC/SCORC (CSI s / CSI u) — save/restore cursor position.
#[test]
fn scosc_scorc() {
    let mut vt = vt100::Parser::default();
    vt.process(b"12345");
    assert_eq!(vt.screen().cursor_position(), (0, 5));
    vt.process(b"\x1b[s\x1b[10;20H");
    assert_eq!(vt.screen().cursor_position(), (9, 19));
    vt.process(b"\x1b[u");
    assert_eq!(vt.screen().cursor_position(), (0, 5));

    // Unlike DECSC/DECRC, SCOSC/SCORC must not save/restore attributes: text
    // written after the restore keeps the attributes in effect at restore
    // time, not the ones saved. Save with default bg, set a green bg, then
    // restore and overwrite — the overwriting cell must still be green (a
    // DECRC alias would wrongly restore the saved default-bg attrs).
    let mut vt = vt100::Parser::default();
    vt.process(b"\x1b[31m\x1b[s\x1b[42mx\x1b[uy");
    let y = vt.screen().cell(0, 0).unwrap();
    assert_eq!(
        y.contents(),
        "y",
        "CSI u restored the cursor to the save spot"
    );
    assert_eq!(
        y.bgcolor(),
        vt100::Color::Idx(2),
        "attrs in effect (green bg) survive CSI u"
    );

    // The parameterized forms are different sequences (DECSLRM) and must not
    // touch the saved cursor.
    let mut vt = vt100::Parser::default();
    vt.process(b"abc\x1b[s\x1b[5;10H\x1b[2s\x1b[u");
    assert_eq!(
        vt.screen().cursor_position(),
        (0, 3),
        "CSI 2 s is not SCOSC"
    );

    // The powerline-prompt shape that motivated the patch: draw a right-
    // aligned segment (save, jump to the right edge, back up, draw, restore)
    // — the cursor must land back just after the left prompt.
    let mut vt = vt100::Parser::new(24, 80, 0);
    vt.process(b"$ ls\x1b[s\x1b[80C\x1b[11D\x1b[7m  master \x1b[0m\x1b[u");
    assert_eq!(vt.screen().cursor_position(), (0, 4));
}

// shellglass: REP (CSI b) — repeat the preceding graphic character.
#[test]
fn rep() {
    let mut vt = vt100::Parser::default();
    vt.process(b"ab\x1b[3b");
    assert_eq!(vt.screen().rows(0, 80).next().unwrap(), "abbbb");

    // Default count is 1.
    let mut vt = vt100::Parser::default();
    vt.process(b"x\x1b[b");
    assert_eq!(vt.screen().rows(0, 80).next().unwrap(), "xx");

    // No preceding graphic character: nothing to repeat, no panic.
    let mut vt = vt100::Parser::default();
    vt.process(b"\x1b[5b");
    assert_eq!(vt.screen().rows(0, 80).next().unwrap(), "");

    // Repeats go through the print path: they wrap like typed characters.
    let mut vt = vt100::Parser::new(24, 10, 0);
    vt.process(b"aaaaaaaaa\x1b[3b");
    assert_eq!(vt.screen().rows(0, 10).next().unwrap(), "aaaaaaaaaa");
    assert_eq!(vt.screen().rows(0, 10).nth(1).unwrap(), "aa");

    // Wide characters repeat as wide characters.
    let mut vt = vt100::Parser::default();
    vt.process("日\x1b[2b".as_bytes());
    assert_eq!(vt.screen().rows(0, 80).next().unwrap(), "日日日");
    assert_eq!(vt.screen().cursor_position(), (0, 6));

    // The repeated char survives cursor movement (data-stream semantics, as
    // in kitty/xterm) and carries the CURRENT attributes, not the original's.
    let mut vt = vt100::Parser::default();
    vt.process(b"q\x1b[5;5H\x1b[31m\x1b[2b");
    assert_eq!(vt.screen().rows(0, 80).nth(4).unwrap(), "    qq");
    assert_eq!(
        vt.screen().cell(4, 4).unwrap().fgcolor(),
        vt100::Color::Idx(1)
    );
}

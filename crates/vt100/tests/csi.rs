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

// shellglass: the cursor-movement aliases stock 0.16.2 dropped.
#[test]
fn movement_aliases() {
    // HVP (CSI f) positions like CUP.
    let mut vt = vt100::Parser::default();
    vt.process(b"\x1b[5;10f");
    assert_eq!(vt.screen().cursor_position(), (4, 9));
    // HPA (CSI `) — column absolute; VPA-style 1-based.
    vt.process(b"\x1b[3`");
    assert_eq!(vt.screen().cursor_position(), (4, 2));
    // HPR (CSI a) — column relative.
    vt.process(b"\x1b[4a");
    assert_eq!(vt.screen().cursor_position(), (4, 6));
    // VPR (CSI e) — row relative.
    vt.process(b"\x1b[2e");
    assert_eq!(vt.screen().cursor_position(), (6, 6));
}

// shellglass: tab stops — HTS (ESC H) sets, TBC (CSI g) clears, HT/CHT/CBT
// navigate the table.
#[test]
fn tab_stops() {
    // Power-on stops every 8 columns, unchanged behavior.
    let mut vt = vt100::Parser::default();
    vt.process(b"ab\tX");
    assert_eq!(vt.screen().rows(0, 80).next().unwrap(), "ab      X");

    // A custom stop: HTS at column 3.
    let mut vt = vt100::Parser::default();
    vt.process(b"\x1b[1;4H\x1bH\r\tX");
    assert_eq!(vt.screen().cursor_position(), (0, 4));
    assert_eq!(vt.screen().rows(0, 80).next().unwrap(), "   X");

    // CBT (CSI Z) goes back through stops; column 0 when none remain.
    let mut vt = vt100::Parser::default();
    vt.process(b"\x1b[1;20H\x1b[Z");
    assert_eq!(vt.screen().cursor_position(), (0, 16));
    vt.process(b"\x1b[3Z");
    assert_eq!(vt.screen().cursor_position(), (0, 0));

    // CHT (CSI I) goes forward through stops.
    let mut vt = vt100::Parser::default();
    vt.process(b"\x1b[2I");
    assert_eq!(vt.screen().cursor_position(), (0, 16));

    // TBC 0 clears the stop under the cursor; the next tab skips past it.
    let mut vt = vt100::Parser::default();
    vt.process(b"\x1b[1;9H\x1b[g\r\t");
    assert_eq!(vt.screen().cursor_position(), (0, 16));

    // TBC 3 clears them all: tab lands on the right margin.
    let mut vt = vt100::Parser::default();
    vt.process(b"\x1b[3g\t");
    assert_eq!(vt.screen().cursor_position(), (0, 79));

    // Widening the screen extends the default stops into the new region.
    let mut vt = vt100::Parser::default();
    vt.screen_mut().set_size(24, 100);
    vt.process(b"\x1b[1;85H\t");
    assert_eq!(vt.screen().cursor_position(), (0, 88));

    // RIS restores the power-on stops.
    let mut vt = vt100::Parser::default();
    vt.process(b"\x1b[3g\x1bc\t");
    assert_eq!(vt.screen().cursor_position(), (0, 8));
}

// shellglass: DECSTR (CSI ! p) — soft terminal reset.
#[test]
fn decstr() {
    let mut vt = vt100::Parser::default();
    // Build up state: content, SGR, DECSC at a colored position, margins +
    // origin mode, hidden cursor, application cursor keys and keypad, a
    // custom tab stop, and a cursor position.
    vt.process(b"hello\x1b[31m\x1b7\x1b[5;10r\x1b[?6h\x1b[?25l\x1b[?1h\x1b=");
    vt.process(b"\x1b[1;4H\x1bH"); // custom tab stop at column 3 (origin row 5)
    vt.process(b"\x1b[2;7H"); // cursor somewhere non-default (origin-relative)
    let pos_before = vt.screen().cursor_position();

    vt.process(b"\x1b[!p");

    // Reset: modes, SGR, margins, origin mode, saved cursor.
    assert!(!vt.screen().hide_cursor(), "cursor visible again");
    assert!(!vt.screen().application_cursor(), "cursor keys normal");
    assert!(!vt.screen().application_keypad(), "keypad numeric");
    // Content and cursor position are untouched.
    assert_eq!(vt.screen().rows(0, 80).next().unwrap(), "hello");
    assert_eq!(vt.screen().cursor_position(), pos_before);
    // SGR is back to normal: new text renders with default attributes.
    vt.process(b"x");
    let cell = vt.screen().cell(pos_before.0, pos_before.1).unwrap();
    assert_eq!(cell.fgcolor(), vt100::Color::Default);
    // Origin mode and margins are gone: CUP 1;1 reaches the true home (with
    // origin mode + top margin 5 it would land on row 4).
    vt.process(b"\x1b[1;1HY");
    assert_eq!(vt.screen().cell(0, 0).unwrap().contents(), "Y");
    // DECSC data is cleared: DECRC goes to home with default attributes, not
    // back to the red save-point.
    vt.process(b"\x1b[32m\x1b8z");
    assert_eq!(vt.screen().cell(0, 0).unwrap().contents(), "z");
    assert_eq!(
        vt.screen().cell(0, 0).unwrap().fgcolor(),
        vt100::Color::Default
    );
    // Tab stops survive (only RIS resets them).
    vt.process(b"\r\t");
    assert_eq!(vt.screen().cursor_position(), (0, 3));

    // The alternate screen is NOT exited by a soft reset.
    let mut vt = vt100::Parser::default();
    vt.process(b"\x1b[?1049h\x1b[!p");
    assert!(vt.screen().alternate_screen(), "alt screen survives DECSTR");
}

// shellglass: CHT/HT with the cursor in the wrap-pending state (col == cols
// after filling a row) must clamp, not panic — found by the quickcheck suite.
#[test]
fn tab_at_wrap_pending_column() {
    let mut vt = vt100::Parser::default();
    vt.process(&[b'x'; 80]);
    vt.process(b"\x1b[I");
    assert_eq!(vt.screen().cursor_position(), (0, 79));
    let mut vt = vt100::Parser::default();
    vt.process(&[b'x'; 80]);
    vt.process(b"\t");
    assert_eq!(vt.screen().cursor_position(), (0, 79));
}

// shellglass: DA queries and XTWINOPS reports/title-stack ops have no render
// effect (the embedding terminal answers the queries) — they must be silent
// no-ops, not unhandled; XTWINOPS ops outside the known-harmless set keep
// reporting.
#[test]
fn da_and_xtwinops_noise_is_deliberately_ignored() {
    #[derive(Default)]
    struct Rec(Vec<(Option<u8>, Vec<u16>, char)>);
    impl vt100::Callbacks for Rec {
        fn unhandled_csi(
            &mut self,
            _: &mut vt100::Screen,
            i1: Option<u8>,
            _: Option<u8>,
            params: &[&[u16]],
            c: char,
        ) {
            self.0.push((i1, params.iter().map(|p| p[0]).collect(), c));
        }
    }
    let mut vt = vt100::Parser::new_with_callbacks(24, 80, 0, Rec::default());
    vt.process(b"before\x1b[c\x1b[0c\x1b[>c");
    vt.process(b"\x1b[11t\x1b[13t\x1b[14t\x1b[16t\x1b[18t\x1b[19t\x1b[21t");
    vt.process(b"\x1b[22;0t\x1b[23;0t after");
    assert_eq!(vt.callbacks().0, vec![], "noise must not report");
    assert_eq!(vt.screen().contents(), "before after");
    // An op outside the known-harmless set still reports (9 = maximize).
    vt.process(b"\x1b[9;1t");
    assert_eq!(vt.callbacks().0, vec![(None, vec![9, 1], 't')]);
}

// shellglass: DECSCUSR (CSI n SP q) cursor style — tracked 0-6, reset by
// DECSTR and RIS, out-of-range values report instead of clobbering state.
#[test]
fn decscusr() {
    let mut vt = vt100::Parser::default();
    assert_eq!(vt.screen().cursor_style(), 0);
    vt.process(b"\x1b[5 q"); // blinking bar (vim insert mode)
    assert_eq!(vt.screen().cursor_style(), 5);
    vt.process(b"\x1b[2 q");
    assert_eq!(vt.screen().cursor_style(), 2);
    // A bare `CSI SP q` (param 0) is "default".
    vt.process(b"\x1b[ q");
    assert_eq!(vt.screen().cursor_style(), 0);
    // Out of range: state untouched (and reported via unhandled_csi).
    vt.process(b"\x1b[3 q\x1b[9 q");
    assert_eq!(vt.screen().cursor_style(), 3);
    // DECSTR resets the style (xterm's reset list includes DECSCUSR).
    vt.process(b"\x1b[6 q\x1b[!p");
    assert_eq!(vt.screen().cursor_style(), 0);
    // RIS resets too.
    vt.process(b"\x1b[6 q\x1bc");
    assert_eq!(vt.screen().cursor_style(), 0);
}

// shellglass: no-op arms, telemetry round 4 — keyboard/query protocol with
// zero render effect. Each must parse cleanly WITHOUT hitting unhandled_csi
// (deliberately-ignored ≠ unhandled) and without disturbing screen state.
#[test]
fn noop_arms_round4() {
    struct Panic;
    impl vt100::Callbacks for Panic {
        fn unhandled_csi(
            &mut self,
            _: &mut vt100::Screen,
            i1: Option<u8>,
            i2: Option<u8>,
            params: &[&[u16]],
            c: char,
        ) {
            panic!("unhandled CSI: {i1:?} {i2:?} {params:?} {c}");
        }
    }

    let mut vt = vt100::Parser::new_with_callbacks(24, 80, 0, Panic);
    vt.process(b"a");
    vt.process(b"\x1b[>4;2m\x1b[>4m\x1b[>m"); // XTMODKEYS (modifyOtherKeys)
    vt.process(b"\x1b[>q"); // XTVERSION
    vt.process(b"\x1b[?u\x1b[>1u\x1b[<u"); // kitty keyboard query/push/pop
    vt.process(b"\x1b[25l"); // undefined bare mode Claude emits on teardown
    vt.process(b"\x1b[?1004h\x1b[?1004l"); // focus-event reporting
    vt.process(b"\x1b[?2031h\x1b[?2031l"); // color-scheme notifications
    vt.process(b"\x1b[?7727h\x1b[?7727l"); // urxvt application-ESC mode
    vt.process(b"\x1b[5n\x1b[6n"); // standard DSR (status, cursor position; ConPTY uses 6)
    vt.process(b"\x1b[?6n\x1b[?15n\x1b[?25n"); // private DSR (DECXCPR, printer, UDK)
                                               // sixel display/scroll modes — 80 (DECSDM), 8452 (cursor right of graphic).
                                               // shellglass mirrors sixel via its own interceptor with a fixed default-
                                               // matching placement, so the parser deliberately ignores these (both forms).
    vt.process(b"\x1b[?80h\x1b[?80l");
    vt.process(b"\x1b[?8452h\x1b[?8452l");
    vt.process(b"b");
    // nothing rendered, nothing moved beyond the two printed glyphs
    assert_eq!(vt.screen().contents(), "ab");
    assert_eq!(vt.screen().cursor_position(), (0, 2));
}

// shellglass: hardening against degenerate/hostile input. Release has no
// overflow-checks, so these run under the debug test build where an unguarded
// u16 subtraction WOULD panic — a regression here fails the test.
#[test]
fn hardening_wide_char_on_narrow_terminal() {
    // A width-2 glyph on a 1-col terminal: `cols - width` would underflow.
    let mut vt = vt100::Parser::new(4, 1, 0);
    vt.process("宽".as_bytes()); // no panic, no wrap
    assert_eq!(vt.screen().size(), (4, 1));
}

#[test]
fn hardening_huge_scroll_and_insert_counts() {
    // A 16-bit CSI count must clamp, not drive 65535 alloc passes.
    let mut vt = vt100::Parser::new(24, 80, 0);
    vt.process(b"hello\x1b[65535L"); // insert_lines (IL)
    vt.process(b"\x1b[65535T"); // scroll_down (SD)
    vt.process(b"\x1b[65535S"); // scroll_up (SU) — already clamped upstream
    assert_eq!(vt.screen().size(), (24, 80));
}

#[test]
fn hardening_zero_size_resize() {
    // A 0-row/0-col resize (bogus CSI 8 t) must floor to 1x1, not underflow.
    let mut vt = vt100::Parser::new(24, 80, 0);
    vt.screen_mut().set_size(0, 0);
    let (rows, cols) = vt.screen().size();
    assert!(rows >= 1 && cols >= 1, "floored to at least 1x1");
}

#[test]
fn truecolor_colon_form_fg_and_bg() {
    // 38:2::r:g:b and 48:2::r:g:b — the colorspace-id colon form, matching 58.
    let mut vt = vt100::Parser::new(4, 20, 0);
    vt.process(b"\x1b[38:2::1:2:3m\x1b[48:2::9:8:7mX");
    let c = vt.screen().cell(0, 0).unwrap();
    assert_eq!(c.fgcolor(), vt100::Color::Rgb(1, 2, 3));
    assert_eq!(c.bgcolor(), vt100::Color::Rgb(9, 8, 7));
}

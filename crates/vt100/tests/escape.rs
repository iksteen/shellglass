mod helpers;

#[test]
fn deckpam() {
    helpers::fixture("deckpam");
}

#[test]
fn ri() {
    helpers::fixture("ri");
}

#[test]
fn ris() {
    helpers::fixture("ris");
}

#[test]
fn vb() {
    struct State {
        vb: usize,
    }

    impl vt100::Callbacks for State {
        fn visual_bell(&mut self, _: &mut vt100::Screen) {
            self.vb += 1;
        }
    }

    let mut parser =
        vt100::Parser::new_with_callbacks(24, 80, 0, State { vb: 0 });
    assert_eq!(parser.callbacks().vb, 0);

    let screen = parser.screen().clone();
    parser.process(b"\x1bg");
    assert_eq!(parser.callbacks().vb, 1);
    assert_eq!(parser.screen().contents_diff(&screen), b"");

    let screen = parser.screen().clone();
    parser.process(b"\x1bg");
    assert_eq!(parser.callbacks().vb, 2);
    assert_eq!(parser.screen().contents_diff(&screen), b"");

    let screen = parser.screen().clone();
    parser.process(b"\x1bg\x1bg\x1bg");
    assert_eq!(parser.callbacks().vb, 5);
    assert_eq!(parser.screen().contents_diff(&screen), b"");

    let screen = parser.screen().clone();
    parser.process(b"foo");
    assert_eq!(parser.callbacks().vb, 5);
    assert_eq!(parser.screen().contents_diff(&screen), b"foo");

    let screen = parser.screen().clone();
    parser.process(b"ba\x1bgr");
    assert_eq!(parser.callbacks().vb, 6);
    assert_eq!(parser.screen().contents_diff(&screen), b"bar");
}

#[test]
fn decsc() {
    helpers::fixture("decsc");
}

#[test]
fn decsc_resize() {
    let mut parser = vt100::Parser::new(24, 80, 0);
    parser.process(b"foo\x1b[20;70Hbar\x1b7");
    assert_eq!(parser.screen().contents(), "foo\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n                                                                     bar");
    assert_eq!(parser.screen().cursor_position(), (19, 72));
    parser.process(b"\x1b[H");
    assert_eq!(parser.screen().cursor_position(), (0, 0));
    parser.screen_mut().set_size(15, 60);
    assert_eq!(parser.screen().contents(), "foo");
    assert_eq!(parser.screen().cursor_position(), (0, 0));
    parser.process(b"y\x1b8z");
    assert_eq!(parser.screen().contents(), "yoo\n\n\n\n\n\n\n\n\n\n\n\n\n\n                                                           z");
    assert_eq!(parser.screen().cursor_position(), (14, 60));
}

// shellglass: a bare ST (`ESC \`) is how vte reports the terminator of an
// OSC/DCS string it already ended — pure syntax, must be silent.
#[test]
fn st_is_deliberately_ignored() {
    struct Panic;
    impl vt100::Callbacks for Panic {
        fn unhandled_escape(
            &mut self,
            _: &mut vt100::Screen,
            i1: Option<u8>,
            i2: Option<u8>,
            b: u8,
        ) {
            panic!("unhandled escape: {i1:?} {i2:?} {b:#x}");
        }
    }
    let mut parser = vt100::Parser::new_with_callbacks(24, 80, 0, Panic);
    parser.process(b"a\x1b]10;?\x1b\\b");
    assert_eq!(parser.screen().contents(), "ab");
}

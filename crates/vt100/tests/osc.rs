mod helpers;

#[test]
fn title_icon_name() {
    #[derive(Default)]
    struct Window {
        title: String,
        icon_name: String,
    }
    impl vt100::Callbacks for Window {
        fn set_window_icon_name(
            &mut self,
            _: &mut vt100::Screen,
            icon_name: &[u8],
        ) {
            self.icon_name =
                std::str::from_utf8(icon_name).unwrap().to_string();
        }
        fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
            self.title = std::str::from_utf8(title).unwrap().to_string();
        }
    }

    let mut parser =
        vt100::Parser::new_with_callbacks(24, 80, 0, Window::default());
    assert_eq!(parser.callbacks().icon_name, "");
    assert_eq!(parser.callbacks().title, "");
    parser.process(b"\x1b]1;icon_name\x07");
    assert_eq!(parser.callbacks().icon_name, "icon_name");
    assert_eq!(parser.callbacks().title, "");
    parser.process(b"\x1b]2;title\x07");
    assert_eq!(parser.callbacks().icon_name, "icon_name");
    assert_eq!(parser.callbacks().title, "title");
    parser.process(b"\x1b]0;both\x07");
    assert_eq!(parser.callbacks().icon_name, "both");
    assert_eq!(parser.callbacks().title, "both");
}

#[test]
fn clipboard() {
    #[derive(Default)]
    struct Clipboard {
        clipboard: std::collections::HashMap<Vec<u8>, Vec<u8>>,
        pasted: Vec<Vec<u8>>,
    }
    impl vt100::Callbacks for Clipboard {
        fn copy_to_clipboard(
            &mut self,
            _: &mut vt100::Screen,
            ty: &[u8],
            data: &[u8],
        ) {
            self.clipboard.insert(ty.to_vec(), data.to_vec());
        }

        fn paste_from_clipboard(&mut self, _: &mut vt100::Screen, ty: &[u8]) {
            self.pasted.push(ty.to_vec());
        }

        fn unhandled_osc(&mut self, _: &mut vt100::Screen, params: &[&[u8]]) {
            panic!("unhandled osc: {params:?}");
        }
    }

    let mut parser =
        vt100::Parser::new_with_callbacks(24, 80, 0, Clipboard::default());
    assert!(parser.callbacks().clipboard.is_empty());
    assert!(parser.callbacks().pasted.is_empty());
    parser.process(b"\x1b]52;c;?\x07");
    assert!(parser.callbacks().clipboard.is_empty());
    assert_eq!(&parser.callbacks().pasted, &[b"c"]);
    parser.process(b"\x1b]52;c;abcdef==\x07");
    assert_eq!(parser.callbacks().clipboard.len(), 1);
    assert_eq!(
        parser.callbacks().clipboard.get(&b"c"[..]),
        Some(&b"abcdef==".to_vec())
    );
    assert_eq!(&parser.callbacks().pasted, &[b"c"]);
}

#[test]
fn unknown_osc() {
    helpers::fixture("unknown_osc");
}

// shellglass: OSC 10/11 set forms override the default fg/bg (both XParseColor
// shapes), 110/111 reset, queries stay silent, and unparseable values report.
#[test]
fn default_colors_track_set_and_reset() {
    #[derive(Default)]
    struct Rec(usize);
    impl vt100::Callbacks for Rec {
        fn unhandled_osc(&mut self, _: &mut vt100::Screen, _: &[&[u8]]) {
            self.0 += 1;
        }
    }
    let mut vt = vt100::Parser::new_with_callbacks(24, 80, 0, Rec::default());
    assert_eq!(vt.screen().default_fg(), None);
    assert_eq!(vt.screen().default_bg(), None);

    vt.process(b"\x1b]11;#300a24\x07\x1b]10;rgb:ff/fe/fd\x1b\\");
    assert_eq!(vt.screen().default_bg(), Some((0x30, 0x0a, 0x24)));
    assert_eq!(vt.screen().default_fg(), Some((0xff, 0xfe, 0xfd)));

    // 16-bit-per-component rgb: (what most theme tools emit) scales to 8.
    vt.process(b"\x1b]11;rgb:1e1e/2e2e/3e3e\x07");
    assert_eq!(vt.screen().default_bg(), Some((0x1e, 0x2e, 0x3e)));
    // #RGB replicates nibbles.
    vt.process(b"\x1b]10;#fa0\x07");
    assert_eq!(vt.screen().default_fg(), Some((0xff, 0xaa, 0x00)));

    // Queries stay silent no-ops; a named color is unparseable → reports.
    vt.process(b"\x1b]10;?\x1b\\\x1b]11;?\x07");
    assert_eq!(vt.callbacks().0, 0);
    vt.process(b"\x1b]11;papayawhip\x07");
    assert_eq!(vt.callbacks().0, 1);
    assert_eq!(vt.screen().default_bg(), Some((0x1e, 0x2e, 0x3e)), "kept");

    // 110/111 reset; RIS wipes both.
    vt.process(b"\x1b]110\x07");
    assert_eq!(vt.screen().default_fg(), None);
    vt.process(b"\x1b]111\x07");
    assert_eq!(vt.screen().default_bg(), None);
    vt.process(b"\x1b]10;#111111\x07\x1b]11;#222222\x07\x1bc");
    assert_eq!(vt.screen().default_fg(), None);
    assert_eq!(vt.screen().default_bg(), None);
}

// shellglass: the window title is screen state — OSC 0/2 set it (OSC 1, icon
// name only, doesn't), XTWINOPS 22/23 save/restore it, RIS wipes it.
#[test]
fn title_tracks_set_stack_and_reset() {
    let mut vt = vt100::Parser::default();
    assert_eq!(vt.screen().title(), "");
    vt.process(b"\x1b]2;vim src/main.rs\x07");
    assert_eq!(vt.screen().title(), "vim src/main.rs");
    vt.process(b"\x1b]0;both\x07\x1b]1;icon-only\x07");
    assert_eq!(
        vt.screen().title(),
        "both",
        "OSC 1 must not touch the title"
    );

    // Push, change, pop — the vim-style save/restore dance.
    vt.process(b"\x1b[22;0t\x1b]2;inner\x07");
    assert_eq!(vt.screen().title(), "inner");
    vt.process(b"\x1b[23;0t");
    assert_eq!(vt.screen().title(), "both");
    // Icon-only push/pop (sub 1) doesn't touch the title stack; a pop from an
    // empty stack is a no-op.
    vt.process(b"\x1b[22;1t\x1b]2;x\x07\x1b[23;1t");
    assert_eq!(vt.screen().title(), "x");
    vt.process(b"\x1b[23t\x1b[23t");
    assert_eq!(vt.screen().title(), "x");

    vt.process(b"\x1bc");
    assert_eq!(vt.screen().title(), "");
}

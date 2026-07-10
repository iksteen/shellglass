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

// shellglass: OSC 10/11 *queries* (vim background detection) are answered by
// the embedding terminal and must be silent; the *set* form really changes
// the local screen and must keep reporting until mirrored (roadmap item 13).
#[test]
fn default_color_queries_silent_set_form_reports() {
    #[derive(Default)]
    struct Rec(Vec<Vec<Vec<u8>>>);
    impl vt100::Callbacks for Rec {
        fn unhandled_osc(&mut self, _: &mut vt100::Screen, params: &[&[u8]]) {
            self.0.push(params.iter().map(|p| p.to_vec()).collect());
        }
    }
    let mut parser =
        vt100::Parser::new_with_callbacks(24, 80, 0, Rec::default());
    parser.process(b"\x1b]10;?\x1b\\\x1b]11;?\x07");
    assert_eq!(parser.callbacks().0, Vec::<Vec<Vec<u8>>>::new());
    parser.process(b"\x1b]11;#300a24\x1b\\\x1b]10;rgb:ff/ff/ff\x07");
    assert_eq!(
        parser.callbacks().0,
        vec![
            vec![b"11".to_vec(), b"#300a24".to_vec()],
            vec![b"10".to_vec(), b"rgb:ff/ff/ff".to_vec()],
        ]
    );
}

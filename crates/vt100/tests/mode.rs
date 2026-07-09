mod helpers;

#[test]
fn modes() {
    helpers::fixture("modes");
}

#[test]
fn alternate_buffer() {
    helpers::fixture("alternate_buffer");
}

// shellglass: synchronized update (DEC private mode 2026) — mode bit plus
// wrapping BSU/ESU counters so a sampling consumer can see edges that opened
// and closed within one read.
#[test]
fn synchronized_update() {
    let mut vt = vt100::Parser::default();
    assert!(!vt.screen().synchronized_update());
    assert_eq!(vt.screen().synchronized_update_starts(), 0);
    assert_eq!(vt.screen().synchronized_update_ends(), 0);

    vt.process(b"\x1b[?2026h");
    assert!(vt.screen().synchronized_update());
    assert_eq!(vt.screen().synchronized_update_starts(), 1);
    assert_eq!(vt.screen().synchronized_update_ends(), 0);

    vt.process(b"\x1b[?2026l");
    assert!(!vt.screen().synchronized_update());
    assert_eq!(vt.screen().synchronized_update_ends(), 1);

    // A full update plus the start of the next, all in one read: the mode bit
    // says "in progress" while the counters expose the completed one.
    vt.process(b"\x1b[?2026hdraw\x1b[?2026l\x1b[?2026h");
    assert!(vt.screen().synchronized_update());
    assert_eq!(vt.screen().synchronized_update_starts(), 3);
    assert_eq!(vt.screen().synchronized_update_ends(), 2);
}

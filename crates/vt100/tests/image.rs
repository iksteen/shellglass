// shellglass: per-cell inline-image tags (Screen::place_image / Cell::image_cell).
// The contract: tags ride vt100's own cell lifecycle — they move with scrolling
// and line insertion/deletion, and die when the cell's contents are overwritten
// or erased — so a consumer can reconstruct any placement's top-left from any
// surviving cell's stored offset, and evict once no cell survives.

use std::num::NonZeroU32;

fn id() -> NonZeroU32 {
    NonZeroU32::new(7).unwrap()
}

fn tag(vt: &vt100::Parser, row: u16, col: u16) -> Option<(u32, u16, u16)> {
    vt.screen()
        .cell(row, col)
        .and_then(vt100::Cell::image_cell)
        .map(|i| (i.id().get(), i.row_off(), i.col_off()))
}

#[test]
fn place_stamps_offsets_and_parks_cursor() {
    let mut vt = vt100::Parser::default();
    vt.process(b"before\x1b[2;3H");
    vt.screen_mut().place_image(id(), 3, 2);
    // Cursor parked at column 0 of the image's last row.
    assert_eq!(vt.screen().cursor_position(), (2, 0));
    // Every covered cell carries the id and its offset within the image.
    for row_off in 0..2 {
        for col_off in 0..3 {
            assert_eq!(
                tag(&vt, 1 + row_off, 2 + col_off),
                Some((7, row_off, col_off))
            );
        }
    }
    // Neighbors are untagged; existing text is untouched.
    assert_eq!(tag(&vt, 1, 1), None);
    assert_eq!(tag(&vt, 0, 2), None);
    assert_eq!(vt.screen().contents(), "before");
}

#[test]
fn overwrite_and_erase_kill_the_tag() {
    let mut vt = vt100::Parser::default();
    vt.screen_mut().place_image(id(), 4, 1);
    // Printing over a covered cell erases the image there — even a space.
    vt.process(b"x ");
    assert_eq!(tag(&vt, 0, 0), None);
    assert_eq!(tag(&vt, 0, 1), None);
    assert_eq!(tag(&vt, 0, 2), Some((7, 0, 2)));
    // EL from column 3 erases the rest.
    vt.process(b"\x1b[4G\x1b[K");
    assert_eq!(tag(&vt, 0, 2), Some((7, 0, 2)));
    assert_eq!(tag(&vt, 0, 3), None);
    // ED wipes the survivor too.
    vt.process(b"\x1b[2J");
    assert_eq!(tag(&vt, 0, 2), None);
}

#[test]
fn tags_ride_scroll_and_line_edits() {
    let mut vt = vt100::Parser::new(4, 10, 0);
    vt.screen_mut().place_image(id(), 2, 2);
    // Scroll one line: the whole image shifts up a row.
    vt.process(b"\x1b[4;1H\r\n");
    assert_eq!(tag(&vt, 0, 0), Some((7, 1, 0)));
    assert_eq!(tag(&vt, 0, 1), Some((7, 1, 1)));
    // (image row 0 scrolled off the top entirely)
    assert_eq!(tag(&vt, 1, 0), None);
    // IL above pushes the survivors down again.
    vt.process(b"\x1b[1;1H\x1b[L");
    assert_eq!(tag(&vt, 0, 0), None);
    assert_eq!(tag(&vt, 1, 0), Some((7, 1, 0)));
}

#[test]
fn taller_than_screen_scrolls_while_placing() {
    let mut vt = vt100::Parser::new(3, 10, 0);
    // A 5-row image on a 3-row screen: placing scrolls row by row, so only
    // the bottom 3 image rows remain, top-aligned like the terminal shows.
    vt.screen_mut().place_image(id(), 1, 5);
    assert_eq!(vt.screen().cursor_position(), (2, 0));
    assert_eq!(tag(&vt, 0, 0), Some((7, 2, 0)));
    assert_eq!(tag(&vt, 1, 0), Some((7, 3, 0)));
    assert_eq!(tag(&vt, 2, 0), Some((7, 4, 0)));
}

#[test]
fn too_wide_image_clips_at_the_right_edge() {
    let mut vt = vt100::Parser::new(4, 10, 0);
    vt.process(b"\x1b[1;9H"); // column 8
    vt.screen_mut().place_image(id(), 5, 1);
    assert_eq!(tag(&vt, 0, 8), Some((7, 0, 0)));
    assert_eq!(tag(&vt, 0, 9), Some((7, 0, 1)));
    // Nothing wrapped to the next row.
    assert_eq!(tag(&vt, 1, 0), None);
}

#[test]
fn alternate_screen_hides_primary_tags() {
    let mut vt = vt100::Parser::default();
    vt.screen_mut().place_image(id(), 2, 1);
    vt.process(b"\x1b[?1049h");
    assert_eq!(tag(&vt, 0, 0), None); // alt grid has no tags
    vt.process(b"\x1b[?1049l");
    assert_eq!(tag(&vt, 0, 0), Some((7, 0, 0))); // primary tags survive
}

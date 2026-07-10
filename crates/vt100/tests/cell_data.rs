// shellglass: the generic per-cell data slot (Cell<T> / Screen::place_data).
// The contract: stamped data rides vt100's own cell lifecycle — it moves with
// scrolling and line insertion/deletion, and dies when the cell's contents
// are overwritten or erased — so a consumer can hang region metadata (e.g. an
// inline-image overlay tag) on cells and reconstruct placements from any
// surviving cell.

type Tag = (u32, u16, u16); // (consumer id, row_off, col_off)
type Parser = vt100::Parser<(), Tag>;

fn parser(rows: u16, cols: u16) -> Parser {
    Parser::new_with_callbacks(rows, cols, 0, ())
}

fn place(vt: &mut Parser, id: u32, w: u16, h: u16) {
    vt.screen_mut().place_data(w, h, |dr, dc| (id, dr, dc));
}

fn tag(vt: &Parser, row: u16, col: u16) -> Option<Tag> {
    vt.screen().cell(row, col).and_then(|c| c.data().copied())
}

#[test]
fn place_stamps_offsets_and_parks_cursor() {
    let mut vt = parser(24, 80);
    vt.process(b"before\x1b[2;3H");
    place(&mut vt, 7, 3, 2);
    // Cursor parked at column 0 of the region's last row.
    assert_eq!(vt.screen().cursor_position(), (2, 0));
    // Every covered cell carries the id and its offset within the region.
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
fn overwrite_and_erase_kill_the_data() {
    let mut vt = parser(24, 80);
    place(&mut vt, 7, 4, 1);
    // Printing over a covered cell drops the slot — even a space.
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
fn data_rides_scroll_and_line_edits() {
    let mut vt = parser(4, 10);
    place(&mut vt, 7, 2, 2);
    // Scroll one line: the whole region shifts up a row.
    vt.process(b"\x1b[4;1H\r\n");
    assert_eq!(tag(&vt, 0, 0), Some((7, 1, 0)));
    assert_eq!(tag(&vt, 0, 1), Some((7, 1, 1)));
    // (region row 0 scrolled off the top entirely)
    assert_eq!(tag(&vt, 1, 0), None);
    // IL above pushes the survivors down again.
    vt.process(b"\x1b[1;1H\x1b[L");
    assert_eq!(tag(&vt, 0, 0), None);
    assert_eq!(tag(&vt, 1, 0), Some((7, 1, 0)));
}

#[test]
fn taller_than_screen_scrolls_while_placing() {
    let mut vt = parser(3, 10);
    // A 5-row region on a 3-row screen: placing scrolls row by row, so only
    // the bottom 3 rows remain, top-aligned like the terminal shows.
    place(&mut vt, 7, 1, 5);
    assert_eq!(vt.screen().cursor_position(), (2, 0));
    assert_eq!(tag(&vt, 0, 0), Some((7, 2, 0)));
    assert_eq!(tag(&vt, 1, 0), Some((7, 3, 0)));
    assert_eq!(tag(&vt, 2, 0), Some((7, 4, 0)));
}

#[test]
fn too_wide_region_clips_at_the_right_edge() {
    let mut vt = parser(4, 10);
    vt.process(b"\x1b[1;9H"); // column 8
    place(&mut vt, 7, 5, 1);
    assert_eq!(tag(&vt, 0, 8), Some((7, 0, 0)));
    assert_eq!(tag(&vt, 0, 9), Some((7, 0, 1)));
    // Nothing wrapped to the next row.
    assert_eq!(tag(&vt, 1, 0), None);
}

#[test]
fn alternate_screen_hides_primary_data() {
    let mut vt = parser(24, 80);
    place(&mut vt, 7, 2, 1);
    vt.process(b"\x1b[?1049h");
    assert_eq!(tag(&vt, 0, 0), None); // alt grid has no data
    vt.process(b"\x1b[?1049l");
    assert_eq!(tag(&vt, 0, 0), Some((7, 0, 0))); // primary data survives
}

// The default dataless parser costs nothing and needs no annotations.
#[test]
fn default_parser_is_dataless() {
    let mut vt = vt100::Parser::default();
    vt.process(b"hi");
    assert_eq!(vt.screen().cell(0, 0).unwrap().data(), None::<&()>);
}

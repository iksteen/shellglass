//! Diff-once, broadcast-to-all live streaming.
//!
//! A backend publishes successive [`Frame`]s to a [`Live`]. `Live` computes the
//! delta from the previous frame **once** and broadcasts a single pre-encoded
//! message to every subscribed viewer — no per-connection recomputation and no
//! retained per-connection state. The wire messages are compact JSON the browser
//! renderer (`viewer.ts`) applies. Cells are columnar (see [`CellBlock`]): a dense
//! text array plus a sparse per-index style map, so plain text costs only its glyphs.
//!
//! - `{"t":"f", w, h, cur, rows:[block,…]}` — a full snapshot (sent to each viewer
//!   on connect, and whenever the screen size changes).
//! - `{"t":"d", cur, rects:[{top,left,w,h, …block}]}` — changed rectangles only.
//!   `rects` address cell-array indices; the viewer re-renders the affected rows
//!   from its own buffer.
//! - `{"t":"b", html}` — an error banner.
//!
//! Rectangles are each row's minimal changed cell-index span, with consecutive rows
//! sharing an identical span merged vertically into one rectangle.
//! ponytail: identical-span merge only; a bounding-rect merge over *overlapping*
//! spans would send fewer rects but more (unchanged) cells — add if a workload wants it.

use crate::model::{Color, Frame, Grid, StyledCell};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use serde::{Serialize, Serializer};
use std::collections::BTreeMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::{broadcast, watch};
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;

/// Broadcast backlog per session. A viewer that falls this many frames behind gets
/// a `Lagged` and is resynced with a fresh full snapshot — never a silent desync.
/// ponytail: fixed; raise if slow viewers resync too often under bursty output.
const BACKLOG: usize = 64;

/// The live publisher for one session. Holds the current full [`Frame`] (for connect
/// snapshots) and a broadcast of pre-encoded delta messages, both coordinated by one
/// mutex so a connecting viewer can atomically snapshot-and-subscribe.
pub struct Live {
    current: Mutex<Arc<Frame>>,
    diffs: broadcast::Sender<Arc<str>>,
}

impl Live {
    /// Create a publisher seeded with `initial` (what a viewer connecting before the
    /// first real frame will see).
    pub fn new(initial: Arc<Frame>) -> Arc<Live> {
        let (diffs, _) = broadcast::channel(BACKLOG);
        Arc::new(Live {
            current: Mutex::new(initial),
            diffs,
        })
    }

    /// Seed a publisher from a backend's frame channel and spawn a task forwarding
    /// every frame into it. Used by the standalone server (the hub feeds its `Live`
    /// from the push stream instead).
    pub fn spawn(mut rx: watch::Receiver<Arc<Frame>>) -> Arc<Live> {
        let live = Live::new(rx.borrow_and_update().clone());
        let fwd = Arc::clone(&live);
        tokio::spawn(async move {
            while rx.changed().await.is_ok() {
                fwd.publish(rx.borrow_and_update().clone());
            }
        });
        live
    }

    /// The current full frame, for an initial server-side paint.
    pub fn current(&self) -> Arc<Frame> {
        Arc::clone(&self.current.lock().unwrap())
    }

    /// Publish the next frame: encode its delta from the current frame once, swap it
    /// in, and broadcast. The send happens under the lock so it's atomic with any
    /// concurrent [`connect`](Live::connect) snapshot+subscribe — a viewer either
    /// snapshots the old frame and receives this delta, or snapshots the new frame
    /// and skips it, never both or neither.
    pub fn publish(&self, next: Arc<Frame>) {
        let mut cur = self.current.lock().unwrap();
        let msg = encode_delta(&cur, &next);
        *cur = next;
        if let Some(msg) = msg {
            let _ = self.diffs.send(msg); // Err only means no viewers — fine.
        }
    }

    /// Subscribe a viewer: an SSE response that emits a full snapshot first, then
    /// each broadcast delta. On `Lagged` (viewer overflowed the backlog) it resyncs
    /// with a fresh full snapshot and carries on.
    pub fn connect(self: &Arc<Self>) -> Response {
        let (full, rx) = {
            let cur = self.current.lock().unwrap();
            (full_message(&cur), self.diffs.subscribe())
        };
        let me = Arc::clone(self);
        let head = tokio_stream::once(Ok::<_, Infallible>(Event::default().data(full)));
        let tail = BroadcastStream::new(rx).map(move |r| {
            let data = match r {
                Ok(msg) => msg.to_string(),
                Err(BroadcastStreamRecvError::Lagged(_)) => {
                    full_message(&me.current.lock().unwrap())
                }
            };
            Ok::<_, Infallible>(Event::default().data(data))
        });
        Sse::new(head.chain(tail))
            .keep_alive(KeepAlive::default())
            .into_response()
    }
}

/// Encode the delta from `cur` to `next`, or `None` if nothing viewers see changed.
fn encode_delta(cur: &Frame, next: &Frame) -> Option<Arc<str>> {
    let msg = match (cur, next) {
        (Frame::Banner(old), Frame::Banner(new)) if old == new => return None,
        (_, Frame::Banner(html)) => banner_message(html),
        (Frame::Screen(a), Frame::Screen(b)) if same_layout(a, b) => diff_message(a, b)?,
        (_, Frame::Screen(b)) => full_message_grid(b),
    };
    Some(Arc::from(msg))
}

/// The full-snapshot message for a frame (banner frames snapshot as a banner).
fn full_message(frame: &Frame) -> String {
    match frame {
        Frame::Screen(g) => full_message_grid(g),
        Frame::Banner(html) => banner_message(html),
    }
}

/// Same screen size ⇒ a diff is applicable; a resize forces a full frame. Compares
/// column count and row count.
fn same_layout(a: &Grid, b: &Grid) -> bool {
    a.cols == b.cols && a.rows.len() == b.rows.len()
}

// ── wire messages ───────────────────────────────────────────────────────────

#[derive(Serialize)]
#[serde(tag = "t")]
enum WireMsg<'a> {
    #[serde(rename = "f")]
    Full {
        w: u16,
        h: usize,
        cur: Option<(u16, u16)>,
        rows: Vec<CellBlock<'a>>,
    },
    #[serde(rename = "d")]
    Diff {
        cur: Option<(u16, u16)>,
        rects: Vec<WireRect<'a>>,
    },
    #[serde(rename = "b")]
    Banner { html: &'a str },
}

#[derive(Serialize, Debug)]
struct WireRect<'a> {
    top: usize,
    left: usize,
    w: usize,
    h: usize,
    #[serde(flatten)]
    block: CellBlock<'a>,
}

/// A run of cells, columnar: text is a dense array (one grapheme per cell, a blank
/// cell as `0` — see [`Text`]), but style is **sparse** — a map from cell index to
/// its non-default attributes (`{f,g,b,d,i,u,n,w}`). Most cells are plain text, so
/// they cost only their glyph; the handful of styled cells each cost one map entry.
/// Empty arrays/maps are omitted, so an all-blank row is `{}`.
/// ponytail: styles aren't deduped — a long same-color run repeats the style object.
/// Add a style table + index if a colorful workload shows up in a payload profile.
#[derive(Serialize, Debug, Default)]
struct CellBlock<'a> {
    #[serde(rename = "t", skip_serializing_if = "Vec::is_empty")]
    text: Vec<Text<'a>>,
    #[serde(rename = "s", skip_serializing_if = "BTreeMap::is_empty")]
    style: BTreeMap<usize, CellStyle>,
}

/// A cell's text in the columnar array: a blank cell is the number `0` (cheaper than
/// `""`), any other cell its glyph string. Blanks dominate a typical screen, so this
/// shrinks full frames noticeably.
#[derive(Debug)]
enum Text<'a> {
    Blank,
    Glyph(&'a str),
}

impl Serialize for Text<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Text::Blank => s.serialize_u8(0),
            Text::Glyph(g) => s.serialize_str(g),
        }
    }
}

/// A cell's non-default style attributes (no text), same compact keys as
/// [`StyledCell`]. Only emitted for cells that aren't plain default text.
#[derive(Serialize, Debug)]
struct CellStyle {
    #[serde(rename = "f", skip_serializing_if = "crate::model::is_default_color")]
    fg: Color,
    #[serde(rename = "g", skip_serializing_if = "crate::model::is_default_color")]
    bg: Color,
    #[serde(rename = "b", skip_serializing_if = "crate::model::is_false")]
    bold: bool,
    #[serde(rename = "d", skip_serializing_if = "crate::model::is_false")]
    dim: bool,
    #[serde(rename = "i", skip_serializing_if = "crate::model::is_false")]
    italic: bool,
    #[serde(rename = "u", skip_serializing_if = "crate::model::is_false")]
    underline: bool,
    #[serde(rename = "n", skip_serializing_if = "crate::model::is_false")]
    inverse: bool,
    #[serde(rename = "w", skip_serializing_if = "crate::model::is_false")]
    wide: bool,
}

/// A cell with no styling: plain default-colored text (or a blank). Such cells carry
/// only their glyph in the `text` column and never appear in the sparse style map.
fn is_plain(c: &StyledCell) -> bool {
    c.fg == Color::Default
        && c.bg == Color::Default
        && !(c.bold || c.dim || c.italic || c.underline || c.inverse || c.wide)
}

/// Encode a sequence of cells into a columnar [`CellBlock`].
fn cell_block<'a>(cells: impl Iterator<Item = &'a StyledCell>) -> CellBlock<'a> {
    let mut block = CellBlock::default();
    for (i, c) in cells.enumerate() {
        block.text.push(if c.text.is_empty() {
            Text::Blank
        } else {
            Text::Glyph(&c.text)
        });
        if !is_plain(c) {
            block.style.insert(
                i,
                CellStyle {
                    fg: c.fg,
                    bg: c.bg,
                    bold: c.bold,
                    dim: c.dim,
                    italic: c.italic,
                    underline: c.underline,
                    inverse: c.inverse,
                    wide: c.wide,
                },
            );
        }
    }
    block
}

fn full_message_grid(g: &Grid) -> String {
    let msg = WireMsg::Full {
        w: g.cols,
        h: g.rows.len(),
        cur: g.cursor,
        rows: g.rows.iter().map(|r| cell_block(r.iter())).collect(),
    };
    serde_json::to_string(&msg).expect("full wire message serializes")
}

fn banner_message(html: &str) -> String {
    serde_json::to_string(&WireMsg::Banner { html }).expect("banner wire message serializes")
}

/// Rectangle diff between two same-size grids. `None` if nothing (cells or cursor)
/// changed.
fn diff_message(a: &Grid, b: &Grid) -> Option<String> {
    let rects = grid_rects(a, b);
    if rects.is_empty() && a.cursor == b.cursor {
        return None; // nothing this viewer would see changed
    }
    Some(
        serde_json::to_string(&WireMsg::Diff {
            cur: b.cursor,
            rects,
        })
        .expect("diff wire message serializes"),
    )
}

/// A shared blank cell, so out-of-range indices (a row that grew/shrank between
/// frames) compare and serialize as an empty cell.
fn blank() -> &'static StyledCell {
    static BLANK: OnceLock<StyledCell> = OnceLock::new();
    BLANK.get_or_init(StyledCell::default)
}

/// The minimal `[lo, hi]` cell-index span that changed between two rows (compared
/// with blank padding past either end), or `None` if identical.
fn row_span(old: &[StyledCell], new: &[StyledCell]) -> Option<(usize, usize)> {
    let len = old.len().max(new.len());
    let mut lo = None;
    let mut hi = 0;
    for i in 0..len {
        if old.get(i).unwrap_or(blank()) != new.get(i).unwrap_or(blank()) {
            lo.get_or_insert(i);
            hi = i;
        }
    }
    lo.map(|l| (l, hi))
}

/// Changed rectangles for the screen: each row's minimal changed span, with runs of
/// consecutive rows sharing an identical span merged into a single rectangle.
fn grid_rects<'a>(old: &Grid, new: &'a Grid) -> Vec<WireRect<'a>> {
    let spans: Vec<Option<(usize, usize)>> = old
        .rows
        .iter()
        .zip(&new.rows)
        .map(|(o, n)| row_span(o, n))
        .collect();

    let mut rects = Vec::new();
    let mut r = 0;
    while r < spans.len() {
        let Some((lo, hi)) = spans[r] else {
            r += 1;
            continue;
        };
        // Extend the rectangle over following rows with the identical span.
        let mut end = r;
        while end + 1 < spans.len() && spans[end + 1] == Some((lo, hi)) {
            end += 1;
        }
        let mut cells = Vec::with_capacity((end - r + 1) * (hi - lo + 1));
        for row in &new.rows[r..=end] {
            for i in lo..=hi {
                cells.push(row.get(i).unwrap_or(blank()));
            }
        }
        rects.push(WireRect {
            top: r,
            left: lo,
            w: hi - lo + 1,
            h: end - r + 1,
            block: cell_block(cells.into_iter()),
        });
        r = end + 1;
    }
    rects
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Color;

    fn cell(c: char) -> StyledCell {
        StyledCell {
            text: c.to_string(),
            ..Default::default()
        }
    }

    /// A grid from rows of text (each char → a plain cell).
    fn grid(rows: &[&str]) -> Grid {
        Grid {
            cols: rows.iter().map(|r| r.chars().count()).max().unwrap_or(0) as u16,
            rows: rows.iter().map(|r| r.chars().map(cell).collect()).collect(),
            cursor: None,
        }
    }

    /// The block's glyphs concatenated (blanks contribute nothing), for assertions.
    fn glyphs(b: &CellBlock) -> String {
        b.text
            .iter()
            .map(|t| match t {
                Text::Glyph(g) => *g,
                Text::Blank => "",
            })
            .collect()
    }

    #[test]
    fn one_changed_cell_is_one_small_rect() {
        let a = grid(&["abc", "def"]);
        let b = grid(&["abc", "dXf"]);
        let rects = grid_rects(&a, &b);
        assert_eq!(rects.len(), 1, "one contiguous change → one rect");
        let r = &rects[0];
        assert_eq!(
            (r.top, r.left, r.w, r.h),
            (1, 1, 1, 1),
            "rect bounds the cell"
        );
        assert_eq!(glyphs(&r.block), "X");
    }

    #[test]
    fn adjacent_rows_with_equal_span_merge_vertically() {
        // Both rows change columns 1..=2 → one 2-high rectangle, not two.
        let a = grid(&["a..z", "a..z"]);
        let b = grid(&["aQQz", "aWWz"]);
        let rects = grid_rects(&a, &b);
        assert_eq!(rects.len(), 1, "equal spans merge: {rects:?}");
        let r = &rects[0];
        assert_eq!((r.top, r.left, r.w, r.h), (0, 1, 2, 2));
        assert_eq!(glyphs(&r.block), "QQWW", "h*w cells, row-major");
    }

    #[test]
    fn scattered_rows_stay_separate_rects() {
        // Rows 0 and 2 change (different spans); row 1 unchanged → two rects.
        let a = grid(&["abcd", "efgh", "ijkl"]);
        let b = grid(&["Xbcd", "efgh", "ijYl"]);
        let rects = grid_rects(&a, &b);
        assert_eq!(rects.len(), 2);
        assert_eq!((rects[0].top, rects[0].left, rects[0].h), (0, 0, 1));
        assert_eq!((rects[1].top, rects[1].left, rects[1].h), (2, 2, 1));
    }

    #[test]
    fn identical_screens_produce_no_message() {
        let a = grid(&["same", "rows"]);
        let b = grid(&["same", "rows"]);
        assert!(same_layout(&a, &b));
        assert!(diff_message(&a, &b).is_none(), "no change → no diff");
    }

    #[test]
    fn cursor_only_move_still_diffs() {
        let mut a = grid(&["abc"]);
        let mut b = grid(&["abc"]);
        a.cursor = Some((0, 0));
        b.cursor = Some((0, 2));
        let msg = diff_message(&a, &b).expect("cursor move is a change");
        assert!(msg.contains("\"cur\":[0,2]"), "carries new cursor: {msg}");
        assert!(msg.contains("\"rects\":[]"), "no cell rects: {msg}");
    }

    #[test]
    fn resize_is_not_a_diff() {
        let a = grid(&["abc"]);
        let b = grid(&["abc", "def"]); // different height
        assert!(!same_layout(&a, &b));
        let c = grid(&["abcd"]); // different width
        assert!(!same_layout(&a, &c), "width change forces a full frame");
    }

    #[test]
    fn full_and_banner_messages_have_expected_shape() {
        let g = grid(&["hi"]);
        let full = full_message_grid(&g);
        assert!(full.starts_with("{\"t\":\"f\""), "{full}");
        // Rows are columnar: a dense text array, no style map for plain text.
        assert!(full.contains(r#""rows":[{"t":["h","i"]}]"#), "{full}");
        let banner = banner_message("oops");
        assert_eq!(banner, "{\"t\":\"b\",\"html\":\"oops\"}");
    }

    #[test]
    fn columnar_block_keeps_text_dense_and_style_sparse() {
        // "a" plain, "B" bold+red, "c" plain → dense text, one sparse style entry.
        let mut b = grid(&["aBc"]);
        b.rows[0][1].bold = true;
        b.rows[0][1].fg = Color::Idx(1);
        let full = full_message_grid(&b);
        assert!(full.contains(r#""t":["a","B","c"]"#), "dense text: {full}");
        assert!(
            full.contains(r#""s":{"1":{"f":1,"b":true}}"#),
            "only the styled cell is in the sparse map: {full}"
        );
    }

    #[test]
    fn blank_cells_encode_as_zero() {
        // 'a' then a blank (empty-text) cell → the blank rides as 0, not "".
        let mut g = grid(&["a"]);
        g.rows[0].push(StyledCell::default());
        let full = full_message_grid(&g);
        assert!(full.contains(r#""t":["a",0]"#), "blank cell is 0: {full}");
    }

    #[test]
    fn full_frame_when_previous_was_a_banner() {
        let prev = Frame::Banner("starting".into());
        let next = Frame::Screen(grid(&["ok"]));
        let msg = encode_delta(&prev, &next).expect("banner → screen is a change");
        assert!(
            msg.starts_with("{\"t\":\"f\""),
            "screen after banner is full: {msg}"
        );
    }

    #[test]
    fn compact_cells_omit_defaults() {
        // A blank cell serializes to {} and a plain letter to {"t":"x"}.
        let g = grid(&["x"]);
        let json = serde_json::to_string(&g.rows[0]).unwrap();
        assert_eq!(json, "[{\"t\":\"x\"}]");
        let blank_json = serde_json::to_string(&StyledCell::default()).unwrap();
        assert_eq!(blank_json, "{}");
        // A 256-color index rides as a bare number; rgb as an array.
        let mut c = cell('y');
        c.fg = Color::Idx(9);
        c.bg = Color::Rgb(1, 2, 3);
        assert_eq!(
            serde_json::to_string(&c).unwrap(),
            "{\"t\":\"y\",\"f\":9,\"g\":[1,2,3]}"
        );
    }
}

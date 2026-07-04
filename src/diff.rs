//! Diff-once, broadcast-to-all live streaming.
//!
//! A publisher feeds successive states into a [`Live`] — the standalone server
//! publishes [`Frame`]s (deltas are computed here, once), the hub publishes the
//! client's already-encoded wire messages (applied to the stored frame, forwarded
//! **verbatim** — no recomputation). Every subscribed viewer gets the same
//! pre-encoded message. The wire messages are compact JSON the browser renderer
//! (`viewer.ts`) applies; the push client emits the identical format, so one
//! encoder/decoder pair covers both hops. Cells are columnar (see [`CellBlock`]):
//! a dense text array plus a sparse per-index style map, so plain text costs only
//! its glyphs.
//!
//! - `{"t":"f", w, h, cur, rows:[block,…]}` — a full snapshot (sent to each viewer
//!   on connect, and whenever the screen size changes).
//! - `{"t":"d", cur, rects:[{top,left,w,h, …block}]}` — changed rectangles only.
//!   `rects` address cell-array indices; the viewer re-renders the affected rows
//!   from its own buffer.
//! - `{"t":"b", html}` — an error banner.
//!
//! Rectangles are each row's minimal changed cell-index span, with consecutive rows
//! sharing an identical span merged vertically into one rectangle. Measured (see
//! `zz_analyze_rect_merging`): looser merging is a dead end — always-merging
//! adjacent rows costs +9% bytes on an 80×24 corpus and +33% at 276×65 (padding
//! scales with width), while an exact-cost greedy merge recovers only 0.3–1.1% and
//! sometimes wins by *splitting* equal-span runs (the sparse style map keys are
//! rect-relative, so tall rects inflate every key on style-dense content). The
//! real remaining fat is the style map itself — runs of identically-styled cells
//! each pay a full `{"n":{...}}` entry; a run-length style encoding is the
//! worthwhile follow-up, not better rectangles.
//!
//! Connecting is **lock-free** (`/s/<id>/events` is public — the id is the read
//! capability — so a connect flood must not be able to stall the publisher): deltas
//! are broadcast tagged with a monotonic sequence number, the current state lives in
//! an [`ArcSwap`] snapshot stamped with the seq it reflects, and a viewer subscribes,
//! loads the snapshot, and skips any delta the snapshot already covers. Each SSE
//! event carries its seq as the `id:` line (`e.lastEventId` in the browser — the
//! native hook for a future `Last-Event-ID` resume).

use arc_swap::ArcSwap;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::BTreeMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::{broadcast, watch};
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;

use crate::model::{Color, Frame, Grid, StyledCell};

/// Broadcast backlog per session. A viewer that falls this many frames behind gets
/// a `Lagged` and is resynced with a fresh full snapshot — never a silent desync.
/// ponytail: fixed; raise if slow viewers resync too often under bursty output.
const BACKLOG: usize = 64;

/// The state a connecting viewer snapshots: the current frame, the sequence number
/// of the last delta it reflects, and a lazily-encoded full message (memoized so a
/// connect flood costs at most one encode per published frame, then pointer clones).
struct State {
    seq: u64,
    frame: Arc<Frame>,
    full: OnceLock<Arc<str>>,
}

impl State {
    fn full_msg(&self) -> Arc<str> {
        self.full
            .get_or_init(|| Arc::from(full_message(&self.frame)))
            .clone()
    }
}

/// The live publisher for one session. Publishing is serialized by `writer` (two
/// pushers for one session contend only with each other); connecting never takes a
/// lock — see the module docs for the seq-reconciliation argument.
pub struct Live {
    state: ArcSwap<State>,
    /// Serializes publishers; connects never touch it.
    writer: Mutex<()>,
    diffs: broadcast::Sender<(u64, Arc<str>)>,
}

impl Live {
    /// Create a publisher seeded with `initial` (what a viewer connecting before the
    /// first real frame will see).
    pub fn new(initial: Arc<Frame>) -> Arc<Live> {
        let (diffs, _) = broadcast::channel(BACKLOG);
        Arc::new(Live {
            state: ArcSwap::from_pointee(State {
                seq: 0,
                frame: initial,
                full: OnceLock::new(),
            }),
            writer: Mutex::new(()),
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

    /// The current full frame, for an initial server-side paint. Lock-free.
    pub fn current(&self) -> Arc<Frame> {
        self.state.load().frame.clone()
    }

    /// Publish the next frame (standalone path): encode its delta from the current
    /// frame once, then commit it. No-op if nothing viewers see changed.
    pub fn publish(&self, next: Arc<Frame>) {
        let guard = self.writer.lock().unwrap();
        let state = self.state.load();
        let Some(msg) = encode_delta(&state.frame, &next) else {
            return;
        };
        self.commit(&guard, state.seq + 1, next, msg);
    }

    /// Publish an already-encoded wire message (hub path): apply it to the stored
    /// frame and forward the received bytes verbatim — no re-diff, no re-encode.
    /// Malformed or out-of-sync messages (a diff while the state is a banner) are
    /// dropped so viewers never receive something they can't apply.
    pub fn publish_wire(&self, msg: &str) {
        // Parse outside the writer lock; only apply+commit are serialized.
        let Ok(wire) = serde_json::from_str::<WireMsgIn>(msg) else {
            return;
        };
        let guard = self.writer.lock().unwrap();
        let state = self.state.load();
        let Some(next) = apply_wire(&state.frame, wire) else {
            return;
        };
        self.commit(&guard, state.seq + 1, Arc::new(next), Arc::from(msg));
    }

    /// Store the new state, THEN broadcast — that order is what makes lock-free
    /// connects sound: a delta sent before a viewer subscribes implies its state was
    /// stored before the viewer's snapshot load, so the snapshot's seq covers it and
    /// the viewer's skip is correct; a delta sent after the subscribe is received,
    /// and is skipped iff the snapshot already includes it.
    fn commit(
        &self,
        _writer: &std::sync::MutexGuard<'_, ()>,
        seq: u64,
        frame: Arc<Frame>,
        msg: Arc<str>,
    ) {
        self.state.store(Arc::new(State {
            seq,
            frame,
            full: OnceLock::new(),
        }));
        let _ = self.diffs.send((seq, msg)); // Err only means no viewers — fine.
    }

    /// Subscribe a viewer: an SSE response that emits a full snapshot first, then
    /// each broadcast delta. **Takes no locks** — subscribe first, snapshot second,
    /// and skip deltas the snapshot already covers (seq ≤ snapshot's). On `Lagged`
    /// (viewer overflowed the backlog) it resyncs with a fresh snapshot, raising the
    /// skip threshold — stale retained deltas are discarded exactly, not replayed.
    pub fn connect(self: &Arc<Self>) -> Response {
        let rx = self.diffs.subscribe();
        let snap = self.state.load_full();
        let full = snap.full_msg();
        let mut threshold = snap.seq;
        let head = tokio_stream::once(Ok::<_, Infallible>(
            Event::default().id(snap.seq.to_string()).data(&*full),
        ));
        let me = Arc::clone(self);
        let tail = BroadcastStream::new(rx).filter_map(move |r| {
            let (seq, data) = match r {
                Ok((seq, msg)) => {
                    if seq <= threshold {
                        return None; // already included in the snapshot we sent
                    }
                    (seq, msg)
                }
                Err(BroadcastStreamRecvError::Lagged(_)) => {
                    let snap = me.state.load_full();
                    threshold = snap.seq;
                    (snap.seq, snap.full_msg())
                }
            };
            Some(Ok::<_, Infallible>(
                Event::default().id(seq.to_string()).data(&*data),
            ))
        });
        Sse::new(head.chain(tail))
            .keep_alive(KeepAlive::default())
            .into_response()
    }
}

/// Encode the delta from `cur` to `next`, or `None` if nothing viewers see changed.
/// Also used by the push client to stream the same deltas to the hub.
pub fn encode_delta(cur: &Frame, next: &Frame) -> Option<Arc<str>> {
    let msg = match (cur, next) {
        (Frame::Banner(old), Frame::Banner(new)) if old == new => return None,
        (_, Frame::Banner(html)) => banner_message(html),
        (Frame::Screen(a), Frame::Screen(b)) if same_layout(a, b) => diff_message(a, b)?,
        (_, Frame::Screen(b)) => full_message_grid(b),
    };
    Some(Arc::from(msg))
}

/// The full-snapshot message for a frame (banner frames snapshot as a banner).
/// Also used by the push client on each (re)connect to seed the hub's matrix.
pub fn full_message(frame: &Frame) -> String {
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

// ── wire decode + apply (the hub side of the client→hub diff stream) ─────────
//
// Owned mirrors of the borrow-encoding wire types above. The hub deserializes each
// received message just enough to keep its own full matrix current (so late-joining
// viewers get a correct snapshot), then forwards the original bytes untouched.

#[derive(Deserialize)]
#[serde(tag = "t")]
enum WireMsgIn {
    #[serde(rename = "f")]
    Full {
        w: u16,
        // The row count is implied by `rows`; the wire's `h` is for the viewer.
        cur: Option<(u16, u16)>,
        rows: Vec<CellBlockIn>,
    },
    #[serde(rename = "d")]
    Diff {
        cur: Option<(u16, u16)>,
        rects: Vec<WireRectIn>,
    },
    #[serde(rename = "b")]
    Banner { html: String },
}

#[derive(Deserialize)]
struct WireRectIn {
    top: usize,
    left: usize,
    w: usize,
    h: usize,
    #[serde(flatten)]
    block: CellBlockIn,
}

#[derive(Deserialize, Default)]
struct CellBlockIn {
    #[serde(rename = "t", default)]
    text: Vec<TextIn>,
    // String keys: JSON object keys always arrive as strings, and the flatten /
    // internally-tagged containers these ride in buffer through serde's Content,
    // which loses serde_json's integer-key conversion. Parsed in decode_block.
    #[serde(rename = "s", default)]
    style: BTreeMap<String, CellStyleIn>,
}

/// Mirror of [`Text`]: the number `0` is a blank cell, a string is a glyph.
enum TextIn {
    Blank,
    Glyph(String),
}

impl<'de> Deserialize<'de> for TextIn {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<TextIn, D::Error> {
        struct TextVisitor;
        impl<'de> Visitor<'de> for TextVisitor {
            type Value = TextIn;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("0 (blank) or a glyph string")
            }
            fn visit_u64<E: de::Error>(self, _: u64) -> Result<TextIn, E> {
                Ok(TextIn::Blank)
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<TextIn, E> {
                Ok(TextIn::Glyph(v.to_string()))
            }
            fn visit_string<E: de::Error>(self, v: String) -> Result<TextIn, E> {
                Ok(TextIn::Glyph(v))
            }
        }
        d.deserialize_any(TextVisitor)
    }
}

/// Mirror of [`CellStyle`]; every field defaults so a sparse entry stays sparse.
#[derive(Deserialize, Default)]
struct CellStyleIn {
    #[serde(rename = "f", default)]
    fg: Color,
    #[serde(rename = "g", default)]
    bg: Color,
    #[serde(rename = "b", default)]
    bold: bool,
    #[serde(rename = "d", default)]
    dim: bool,
    #[serde(rename = "i", default)]
    italic: bool,
    #[serde(rename = "u", default)]
    underline: bool,
    #[serde(rename = "n", default)]
    inverse: bool,
    #[serde(rename = "w", default)]
    wide: bool,
}

/// Materialize a columnar block into cells (mirror of `viewer.ts::decodeBlock`).
fn decode_block(block: CellBlockIn) -> Vec<StyledCell> {
    let mut style = block.style;
    block
        .text
        .into_iter()
        .enumerate()
        .map(|(i, t)| {
            let text = match t {
                TextIn::Blank => String::new(),
                TextIn::Glyph(g) => g,
            };
            match style.remove(&i.to_string()) {
                Some(s) => StyledCell {
                    text,
                    fg: s.fg,
                    bg: s.bg,
                    bold: s.bold,
                    dim: s.dim,
                    italic: s.italic,
                    underline: s.underline,
                    inverse: s.inverse,
                    wide: s.wide,
                },
                None => StyledCell {
                    text,
                    ..Default::default()
                },
            }
        })
        .collect()
}

/// Apply a decoded wire message to the previous frame, yielding the new one — the
/// state transition `viewer.ts` performs, mirrored so the hub's matrix stays in
/// lockstep with every browser. `None` = drop the message (a diff arriving while
/// the state is a banner is a desync; forwarding it would corrupt viewers too).
fn apply_wire(prev: &Frame, msg: WireMsgIn) -> Option<Frame> {
    match msg {
        WireMsgIn::Full { w, cur, rows } => Some(Frame::Screen(Grid {
            cols: w,
            rows: rows.into_iter().map(decode_block).collect(),
            cursor: cur,
        })),
        WireMsgIn::Banner { html } => Some(Frame::Banner(html)),
        WireMsgIn::Diff { cur, rects } => {
            let Frame::Screen(grid) = prev else {
                return None;
            };
            let mut grid = grid.clone();
            for rect in rects {
                let cells = decode_block(rect.block);
                for dy in 0..rect.h {
                    let Some(row) = grid.rows.get_mut(rect.top + dy) else {
                        continue; // out-of-range row: same_layout should prevent this
                    };
                    for dx in 0..rect.w {
                        let cell = cells.get(dy * rect.w + dx).cloned().unwrap_or_default();
                        let i = rect.left + dx;
                        if i < row.len() {
                            row[i] = cell;
                        } else {
                            // Mirror viewer.ts: a jagged row can grow — pad then push.
                            while row.len() < i {
                                row.push(StyledCell::default());
                            }
                            row.push(cell);
                        }
                    }
                }
            }
            grid.cursor = cur;
            Some(Frame::Screen(grid))
        }
    }
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

    /// Wire-cost analysis harness, inert without SG_CORPUS. Replays real terminal
    /// recordings through vt100 with the production 33ms frame coalescing and
    /// compares rect strategies by exact encoded bytes — the measurement tool for
    /// any future wire-format change (e.g. run-length style encoding).
    ///
    /// Record a corpus session (name.tm + name.raw pairs in a directory):
    ///   script -q --log-timing name.tm --log-out name.raw \
    ///     -c 'stty cols 80 rows 24; <the workload>'
    /// Run: SG_CORPUS=<dir> [SG_SIZE=80x24] cargo test zz_analyze -- --nocapture
    ///
    /// Findings (2026-07, 80×24 + 276×65 corpora: htop, vim, logs, cargo, typing,
    /// progress bars, watch): always-merging adjacent rows costs +9%/+33% bytes;
    /// exact-cost greedy merging (incl. bridging unchanged rows) recovers only
    /// 0.3–1.1% and often prefers SPLITTING tall rects because rect-relative style
    /// keys inflate on style-dense content. Current conservative merging is within
    /// ~1% of greedy-optimal; the style map, not rect geometry, is where the
    /// remaining bytes are.
    #[test]
    fn zz_analyze_rect_merging() {
        let dir = std::env::var("SG_CORPUS").unwrap_or_default();
        if dir.is_empty() {
            return;
        }

        // Encoded size of one rect (its share of the diff message).
        fn rect_bytes(r: &WireRect) -> usize {
            serde_json::to_string(r).unwrap().len() + 1 // +1 for the list comma
        }
        // Build a bounding rect for rows top..=bot spanning cell cols lo..=hi.
        fn bounding<'a>(g: &'a Grid, top: usize, bot: usize, lo: usize, hi: usize) -> WireRect<'a> {
            let mut cells = Vec::new();
            for row in &g.rows[top..=bot] {
                for i in lo..=hi {
                    cells.push(row.get(i).unwrap_or(blank()));
                }
            }
            WireRect {
                top,
                left: lo,
                w: hi - lo + 1,
                h: bot - top + 1,
                block: cell_block(cells.into_iter()),
            }
        }

        // Greedy: accumulate a rect; merge the next changed row (bridging up to
        // `gap` unchanged rows as padding) when the merged encoding is no larger
        // than the pieces. `gap` 0 = adjacent only.
        fn greedy<'a>(old: &Grid, new: &'a Grid, gap: usize) -> Vec<WireRect<'a>> {
            let spans: Vec<Option<(usize, usize)>> = old
                .rows
                .iter()
                .zip(&new.rows)
                .map(|(o, n)| row_span(o, n))
                .collect();
            let mut out: Vec<WireRect> = Vec::new();
            // (top, bot, lo, hi) of the rect being accumulated.
            let mut acc: Option<(usize, usize, usize, usize)> = None;
            for (r, span) in spans.iter().enumerate() {
                let Some((lo2, hi2)) = span else { continue };
                acc = Some(match acc.take() {
                    None => (r, r, *lo2, *hi2),
                    Some((top, bot, lo, hi)) => {
                        if r - bot <= gap + 1 {
                            let m = bounding(new, top, r, lo.min(*lo2), hi.max(*hi2));
                            let a = bounding(new, top, bot, lo, hi);
                            let b = bounding(new, r, r, *lo2, *hi2);
                            if rect_bytes(&m) <= rect_bytes(&a) + rect_bytes(&b) {
                                (top, r, lo.min(*lo2), hi.max(*hi2))
                            } else {
                                out.push(a);
                                (r, r, *lo2, *hi2)
                            }
                        } else {
                            out.push(bounding(new, top, bot, lo, hi));
                            (r, r, *lo2, *hi2)
                        }
                    }
                });
            }
            if let Some((top, bot, lo, hi)) = acc {
                out.push(bounding(new, top, bot, lo, hi));
            }
            out
        }

        // Always-merge adjacent changed rows (no cost check), for comparison.
        fn always<'a>(old: &Grid, new: &'a Grid) -> Vec<WireRect<'a>> {
            let spans: Vec<Option<(usize, usize)>> = old
                .rows
                .iter()
                .zip(&new.rows)
                .map(|(o, n)| row_span(o, n))
                .collect();
            let mut out = Vec::new();
            let mut acc: Option<(usize, usize, usize, usize)> = None;
            for (r, span) in spans.iter().enumerate() {
                let Some((lo2, hi2)) = span else {
                    if let Some((top, bot, lo, hi)) = acc.take() {
                        out.push(bounding(new, top, bot, lo, hi));
                    }
                    continue;
                };
                acc = Some(match acc.take() {
                    None => (r, r, *lo2, *hi2),
                    Some((top, _, lo, hi)) => (top, r, lo.min(*lo2), hi.max(*hi2)),
                });
            }
            if let Some((top, bot, lo, hi)) = acc {
                out.push(bounding(new, top, bot, lo, hi));
            }
            out
        }

        fn total(rects: &[WireRect]) -> (usize, usize) {
            (rects.iter().map(rect_bytes).sum(), rects.len())
        }

        println!(
            "{:<10} {:>7} | {:>9} {:>6} | {:>9} {:>6} | {:>9} {:>6} | {:>9} {:>6}",
            "session",
            "frames",
            "S0 bytes",
            "rects",
            "S1 bytes",
            "rects",
            "S2 bytes",
            "rects",
            "S3 bytes",
            "rects"
        );
        let (mut t0, mut t1, mut t2, mut t3) = ((0, 0), (0, 0), (0, 0), (0, 0));
        let add = |t: &mut (usize, usize), v: (usize, usize)| {
            t.0 += v.0;
            t.1 += v.1;
        };
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|e| e.to_str()) != Some("raw") {
                continue;
            }
            let name = path.file_stem().unwrap().to_string_lossy().to_string();
            let raw = std::fs::read(&path).unwrap();
            let timing = std::fs::read_to_string(path.with_extension("tm")).unwrap();

            // Replay: cut frames at ≥33ms of accumulated delay, like MIN_FRAME.
            let (rows, cols) = std::env::var("SG_SIZE")
                .ok()
                .and_then(|s| {
                    let (c, r) = s.split_once('x')?;
                    Some((r.parse().ok()?, c.parse().ok()?))
                })
                .unwrap_or((24, 80));
            let mut parser = vt100::Parser::new(rows, cols, 0);
            let mut frames: Vec<Grid> = vec![crate::parse::grid_from_screen(parser.screen())];
            let mut pos = 0usize;
            let mut since = 0.0f64;
            for line in timing.lines() {
                let mut it = line.split_whitespace();
                let (Some(d), Some(n)) = (it.next(), it.next()) else {
                    continue;
                };
                let (d, n): (f64, usize) = (d.parse().unwrap(), n.parse().unwrap());
                since += d;
                if since >= 0.033 {
                    frames.push(crate::parse::grid_from_screen(parser.screen()));
                    since = 0.0;
                }
                parser.process(&raw[pos..(pos + n).min(raw.len())]);
                pos += n;
            }
            frames.push(crate::parse::grid_from_screen(parser.screen()));

            let (mut s0, mut s1, mut s2, mut s3) = ((0, 0), (0, 0), (0, 0), (0, 0));
            let mut nframes = 0;
            for pair in frames.windows(2) {
                let (a, b) = (&pair[0], &pair[1]);
                if !same_layout(a, b) {
                    continue; // resize → full frame for every strategy
                }
                nframes += 1;
                add(&mut s0, total(&grid_rects(a, b)));
                add(&mut s1, total(&always(a, b)));
                add(&mut s2, total(&greedy(a, b, 0)));
                add(&mut s3, total(&greedy(a, b, 3)));
            }
            println!(
                "{:<10} {:>7} | {:>9} {:>6} | {:>9} {:>6} | {:>9} {:>6} | {:>9} {:>6}",
                name, nframes, s0.0, s0.1, s1.0, s1.1, s2.0, s2.1, s3.0, s3.1
            );
            add(&mut t0, s0);
            add(&mut t1, s1);
            add(&mut t2, s2);
            add(&mut t3, s3);
        }
        println!(
            "{:<10} {:>7} | {:>9} {:>6} | {:>9} {:>6} | {:>9} {:>6} | {:>9} {:>6}",
            "TOTAL", "", t0.0, t0.1, t1.0, t1.1, t2.0, t2.1, t3.0, t3.1
        );
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

    // ── decode/apply round trips (hub matrix must mirror every viewer) ────────

    /// Apply an encoded wire message string onto a frame, as the hub does.
    fn apply(prev: &Frame, msg: &str) -> Option<Frame> {
        apply_wire(prev, serde_json::from_str::<WireMsgIn>(msg).unwrap())
    }

    /// Grid equality up to trailing blank cells per row (a diff that shrinks a row
    /// leaves explicit blanks where the origin simply has a shorter row — same
    /// rendering, different cell count).
    fn assert_grid_equiv(a: &Grid, b: &Grid) {
        let trim = |g: &Grid| {
            let mut g = g.clone();
            for row in &mut g.rows {
                while row.last() == Some(&StyledCell::default()) {
                    row.pop();
                }
            }
            g
        };
        assert_eq!(trim(a), trim(b));
    }

    #[test]
    fn full_message_roundtrips_through_apply() {
        // Wide + styled + blank cells all survive encode → decode → grid.
        let mut g = grid(&["a世c", "xyz"]);
        g.rows[0][1].wide = true;
        g.rows[0][2].fg = Color::Idx(9);
        g.rows[0][2].bg = Color::Rgb(1, 2, 3);
        g.rows[0][2].bold = true;
        g.rows[1].push(StyledCell::default()); // trailing blank rides as 0
        g.cursor = Some((1, 2));
        let prev = Frame::Banner("old".into());
        let applied = apply(&prev, &full_message_grid(&g)).expect("full applies");
        assert_eq!(applied, Frame::Screen(g));
    }

    #[test]
    fn diff_message_roundtrips_through_apply() {
        // Styled change + jagged row growth + cursor move, applied at the "hub".
        // Diffs only ever happen between same-layout grids, so pin cols.
        let mut a = grid(&["hello", "sh"]);
        let mut b = grid(&["heLLo", "sh $ ls -la"]);
        a.cols = 11;
        b.rows[0][2].fg = Color::Idx(2);
        b.rows[0][2].inverse = true;
        a.cursor = Some((0, 0));
        b.cursor = Some((1, 10));
        let msg = diff_message(&a, &b).expect("changes → diff");
        let applied = apply(&Frame::Screen(a), &msg).expect("diff applies");
        let Frame::Screen(applied) = applied else {
            panic!("diff yields a screen")
        };
        assert_grid_equiv(&applied, &b);
    }

    #[test]
    fn diff_apply_handles_row_shrink() {
        // The new row is shorter; the diff writes explicit blanks over the tail.
        // (Same layout — only the row contents shrink, not the grid.)
        let a = grid(&["longline"]);
        let mut b = grid(&["log"]);
        b.cols = a.cols;
        let msg = diff_message(&a, &b).expect("shrink → diff");
        let Frame::Screen(applied) = apply(&Frame::Screen(a), &msg).unwrap() else {
            panic!("screen")
        };
        assert_grid_equiv(&applied, &b);
    }

    #[test]
    fn diff_on_banner_is_dropped() {
        let a = grid(&["abc"]);
        let b = grid(&["abX"]);
        let msg = diff_message(&a, &b).unwrap();
        assert!(
            apply(&Frame::Banner("waiting".into()), &msg).is_none(),
            "a diff can't apply to a banner — must be dropped, not forwarded"
        );
    }

    #[test]
    fn publish_wire_updates_current_and_forwards_verbatim() {
        let live = Live::new(Arc::new(Frame::Banner("waiting".into())));
        let mut rx = live.diffs.subscribe();

        let g = grid(&["hi"]);
        let full = full_message_grid(&g);
        live.publish_wire(&full);
        assert_eq!(*live.current(), Frame::Screen(g.clone()), "matrix updated");
        let (seq, fwd) = rx.try_recv().expect("full forwarded");
        assert_eq!((seq, &*fwd), (1, full.as_str()), "verbatim bytes, seq 1");

        // A diff advances both the matrix and the seq, still verbatim.
        let g2 = grid(&["hX"]);
        let dmsg = diff_message(&g, &g2).unwrap();
        live.publish_wire(&dmsg);
        assert_eq!(*live.current(), Frame::Screen(g2), "diff applied to matrix");
        let (seq, fwd) = rx.try_recv().expect("diff forwarded");
        assert_eq!((seq, &*fwd), (2, dmsg.as_str()));

        // Garbage and out-of-sync messages are dropped, not forwarded.
        live.publish_wire("not json");
        let live2 = Live::new(Arc::new(Frame::Banner("waiting".into())));
        let mut rx2 = live2.diffs.subscribe();
        live2.publish_wire(&dmsg); // diff while state is a banner
        assert!(rx.try_recv().is_err(), "garbage not forwarded");
        assert!(rx2.try_recv().is_err(), "diff-on-banner not forwarded");
    }

    #[test]
    fn snapshot_seq_covers_prior_deltas() {
        // The lock-free connect contract: a snapshot loaded after publishes reports
        // the seq of the last delta, so a viewer skips everything ≤ it.
        let live = Live::new(Arc::new(Frame::Screen(grid(&["a"]))));
        live.publish(Arc::new(Frame::Screen(grid(&["b"]))));
        live.publish(Arc::new(Frame::Screen(grid(&["c"]))));
        let snap = live.state.load();
        assert_eq!(snap.seq, 2, "two deltas published");
        assert_eq!(*snap.frame, Frame::Screen(grid(&["c"])));
        // The memoized full reflects the latest state.
        assert!(snap.full_msg().contains("\"c\""));
        // An unchanged publish is a no-op (no seq bump, no message).
        live.publish(Arc::new(Frame::Screen(grid(&["c"]))));
        assert_eq!(live.state.load().seq, 2);
    }
}

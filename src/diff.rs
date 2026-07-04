//! Diff-once, broadcast-to-all live streaming.
//!
//! A publisher feeds successive states into a [`Live`] — the standalone server
//! publishes [`Frame`]s (deltas are computed here, once), the hub publishes the
//! client's already-encoded wire messages (applied to the stored frame, forwarded
//! **verbatim** — no recomputation). Every subscribed viewer gets the same
//! pre-encoded message. The wire messages are compact JSON the browser renderer
//! (`viewer.ts`) applies; the push client emits the identical format, so one
//! encoder/decoder pair covers both hops.
//!
//! - `{"t":"f", w, h, c?, r:[block,…]}` — a full snapshot (sent to each viewer
//!   on connect, and whenever the screen size changes). `c` = cursor; absent =
//!   hidden.
//! - `{"t":"d", c?, r?:[[row, left, text, style?],…]}` — changed lines: row
//!   index, the cell index the span starts at, then the block. On diff-family
//!   messages `c` is TRI-STATE (absent = unchanged, null = hidden, [row, col] =
//!   moved) and an empty `r` is omitted (cursor-only moves).
//! - `{"t":"c", c?, r:[row, left, "…"], …style}` — a uniform span: one cell per
//!   codepoint, the flattened style applying to every cell. The hottest diffs
//!   (spinner ticks, typing echo, appended log lines) take this form.
//! - `{"t":"l", c?, r:[row, left, text, style?]}` — a single changed line that
//!   isn't uniform (mixed styles, blanks-as-0, or cluster cells).
//! - `{"t":"b", html}` — an error banner.
//!
//! A block (see [`CellBlock`]) is positional `[text]` / `[text, style]`: `text` is
//! one cell per codepoint in merged strings (`["…"]` = one multi-codepoint-grapheme
//! cell; `0` = an empty-text cell, vestigial now that blanks are canonicalized to
//! spaces at the parse boundary, but still decoded), `style` is
//! `[start, len, {attrs}]` runs with `1`-flags. Diffs are per-line minimal spans — measured (see
//! `zz_measure_wire_cost` history), merging lines into rectangles never paid.
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
        #[serde(rename = "c", skip_serializing_if = "Option::is_none")]
        cur: Option<(u16, u16)>,
        #[serde(rename = "r")]
        rows: Vec<CellBlock<'a>>,
    },
    #[serde(rename = "d")]
    Diff {
        /// Tri-state: absent = cursor unchanged, `null` = became hidden,
        /// `[row, col]` = moved. (Fulls are absolute: absent there = hidden.)
        #[serde(rename = "c", skip_serializing_if = "Option::is_none")]
        cur: Option<Option<(u16, u16)>>,
        #[serde(rename = "r", skip_serializing_if = "Vec::is_empty")]
        rows: Vec<WireRow<'a>>,
    },
    /// A uniform span — the hottest diffs (spinner ticks, typing echo, appended
    /// log lines) change one line where every cell shares one style: `r` is the
    /// bare `[row, left, "…"]` tuple, the string is ONE CELL PER CODEPOINT, and
    /// the style flattens into the message itself, applying to every cell
    /// (`f,g,b,d,i,u,n,w` don't collide with `t,c,r`).
    #[serde(rename = "c")]
    Cell {
        #[serde(rename = "c", skip_serializing_if = "Option::is_none")]
        cur: Option<Option<(u16, u16)>>,
        r: (usize, usize, String),
        #[serde(flatten)]
        style: Option<CellStyle>,
    },
    /// A single changed line: `r` is the bare `[row, left, entries, runs?]` tuple.
    #[serde(rename = "l")]
    Line {
        #[serde(rename = "c", skip_serializing_if = "Option::is_none")]
        cur: Option<Option<(u16, u16)>>,
        r: WireRow<'a>,
    },
    #[serde(rename = "b")]
    Banner { html: &'a str },
}

/// One changed line, positional — `left` is the cell index the span starts at.
/// Two forms, distinguished by the third element's type:
/// - `[row, left, text-entries, style-runs?]` — the general line span.
/// - `[row, left, "x", {style}?]` — a single changed cell holding one codepoint
///   (a bare wire string is always one cell per codepoint; multi-codepoint
///   grapheme cells use the `["…"]` entry form in a line span).
///
/// Line-only and tuple-framed on purpose — measured (see `zz_measure_wire_cost`),
/// merging lines into rectangles never paid (padding scales with width), and the
/// object envelope was pure overhead once the shape was fixed.
#[derive(Debug)]
enum WireRow<'a> {
    Line {
        r: usize,
        l: usize,
        block: CellBlock<'a>,
    },
    Cell {
        r: usize,
        l: usize,
        cell: &'a StyledCell,
    },
}

#[cfg(test)]
impl WireRow<'_> {
    fn pos(&self) -> (usize, usize) {
        match self {
            WireRow::Line { r, l, .. } | WireRow::Cell { r, l, .. } => (*r, *l),
        }
    }
    fn text(&self) -> String {
        match self {
            WireRow::Line { block, .. } => block
                .text
                .iter()
                .map(|t| match t {
                    Text::Run(g) => g.as_str(),
                    Text::Cluster(g) => g,
                    Text::Blank => "",
                })
                .collect(),
            WireRow::Cell { cell, .. } => cell.text.clone(),
        }
    }
}

impl Serialize for WireRow<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        match self {
            WireRow::Line { r, l, block } => {
                let n = if block.style.is_empty() { 3 } else { 4 };
                let mut seq = s.serialize_seq(Some(n))?;
                seq.serialize_element(r)?;
                seq.serialize_element(l)?;
                seq.serialize_element(&block.text)?;
                if !block.style.is_empty() {
                    seq.serialize_element(&block.style)?;
                }
                seq.end()
            }
            WireRow::Cell { r, l, cell } => {
                let plain = is_plain(cell);
                let mut seq = s.serialize_seq(Some(if plain { 3 } else { 4 }))?;
                seq.serialize_element(r)?;
                seq.serialize_element(l)?;
                seq.serialize_element(&cell.text)?;
                if !plain {
                    seq.serialize_element(&cell_style(cell))?;
                }
                seq.end()
            }
        }
    }
}

/// A run of cells, columnar and positional: `[text]` or `[text, style]`. `text` is
/// dense: a string is one cell per *codepoint* (consecutive single-codepoint glyphs
/// merged — `"foo"` is three cells), `0` is a blank cell, and a one-element array
/// `["…"]` is a single cell holding a multi-codepoint grapheme (combining marks),
/// which a merged string could not represent unambiguously. `style` is run-length:
/// `[start, len, style]` triples over the same cell indices, non-default styles
/// only, omitted entirely when the run is plain.
#[derive(Debug, Default)]
struct CellBlock<'a> {
    text: Vec<Text<'a>>,
    style: Vec<(usize, usize, CellStyle)>,
}

impl Serialize for CellBlock<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        let n = if self.style.is_empty() { 1 } else { 2 };
        let mut seq = s.serialize_seq(Some(n))?;
        seq.serialize_element(&self.text)?;
        if !self.style.is_empty() {
            seq.serialize_element(&self.style)?;
        }
        seq.end()
    }
}

/// A `t` entry — see [`CellBlock`]. Blanks stay separate `0` entries rather than
/// NULs inside the run: JSON escapes NUL as `\\u0000` (6 bytes), `,0` is 2.
#[derive(Debug)]
enum Text<'a> {
    Blank,
    /// One cell per codepoint.
    Run(String),
    /// A single cell whose text is a multi-codepoint grapheme.
    Cluster(&'a str),
}

impl Serialize for Text<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Text::Blank => s.serialize_u8(0),
            Text::Run(g) => s.serialize_str(g),
            Text::Cluster(g) => {
                use serde::ser::SerializeSeq;
                let mut seq = s.serialize_seq(Some(1))?;
                seq.serialize_element(g)?;
                seq.end()
            }
        }
    }
}

/// Serialize a flag as the number `1` — 3 bytes cheaper than `true`, and only
/// ever called for set flags (false is skipped entirely: absent = false).
fn flag<S: Serializer>(_: &bool, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_u8(1)
}

/// A style-run's attributes, same compact keys as [`StyledCell`]. Only emitted for
/// cells that aren't plain default text; set flags ride as `1`, unset are absent.
#[derive(Serialize, Debug, PartialEq)]
struct CellStyle {
    #[serde(rename = "f", skip_serializing_if = "crate::model::is_default_color")]
    fg: Color,
    #[serde(rename = "g", skip_serializing_if = "crate::model::is_default_color")]
    bg: Color,
    #[serde(
        rename = "b",
        skip_serializing_if = "crate::model::is_false",
        serialize_with = "flag"
    )]
    bold: bool,
    #[serde(
        rename = "d",
        skip_serializing_if = "crate::model::is_false",
        serialize_with = "flag"
    )]
    dim: bool,
    #[serde(
        rename = "i",
        skip_serializing_if = "crate::model::is_false",
        serialize_with = "flag"
    )]
    italic: bool,
    #[serde(
        rename = "u",
        skip_serializing_if = "crate::model::is_false",
        serialize_with = "flag"
    )]
    underline: bool,
    #[serde(
        rename = "n",
        skip_serializing_if = "crate::model::is_false",
        serialize_with = "flag"
    )]
    inverse: bool,
    #[serde(
        rename = "w",
        skip_serializing_if = "crate::model::is_false",
        serialize_with = "flag"
    )]
    wide: bool,
}

/// A cell with no styling: plain default-colored text (or a blank). Such cells carry
/// only their glyph in the `text` column and never appear in the sparse style map.
fn is_plain(c: &StyledCell) -> bool {
    c.fg == Color::Default
        && c.bg == Color::Default
        && !(c.bold || c.dim || c.italic || c.underline || c.inverse || c.wide)
}

fn cell_style(c: &StyledCell) -> CellStyle {
    CellStyle {
        fg: c.fg,
        bg: c.bg,
        bold: c.bold,
        dim: c.dim,
        italic: c.italic,
        underline: c.underline,
        inverse: c.inverse,
        wide: c.wide,
    }
}

/// Encode a sequence of cells into a columnar [`CellBlock`]: consecutive
/// single-codepoint glyphs coalesce into one string, and consecutive cells with
/// the same non-default style coalesce into one `[start, len, style]` run.
fn cell_block<'a>(cells: impl Iterator<Item = &'a StyledCell>) -> CellBlock<'a> {
    let mut block = CellBlock::default();
    let mut run = String::new(); // pending single-codepoint glyph run
    let mut style_run: Option<(usize, usize, CellStyle)> = None;
    for (i, c) in cells.enumerate() {
        // Text column.
        if c.text.is_empty() {
            if !run.is_empty() {
                block.text.push(Text::Run(std::mem::take(&mut run)));
            }
            block.text.push(Text::Blank);
        } else if c.text.chars().nth(1).is_none() {
            run.push_str(&c.text);
        } else {
            if !run.is_empty() {
                block.text.push(Text::Run(std::mem::take(&mut run)));
            }
            block.text.push(Text::Cluster(&c.text));
        }
        // Style runs. A plain cell ends any open run; styles compare per-cell, so
        // a run is always over consecutive indices.
        if is_plain(c) {
            if let Some(r) = style_run.take() {
                block.style.push(r);
            }
        } else {
            let s = cell_style(c);
            style_run = Some(match style_run.take() {
                Some((start, len, prev)) if prev == s => (start, len + 1, prev),
                Some(r) => {
                    block.style.push(r);
                    (i, 1, s)
                }
                None => (i, 1, s),
            });
        }
    }
    if !run.is_empty() {
        block.text.push(Text::Run(run));
    }
    if let Some(r) = style_run {
        block.style.push(r);
    }
    block
}

/// A block that is exactly one merged run (no blanks-as-0, no clusters) whose
/// style is empty or one run covering every cell — squashable to the uniform
/// `"t":"c"` form.
fn uniform(block: &CellBlock) -> bool {
    let [Text::Run(run)] = block.text.as_slice() else {
        return false;
    };
    match block.style.as_slice() {
        [] => true,
        [(0, n, _)] => *n == run.chars().count(),
        _ => false,
    }
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

/// Per-line diff between two same-size grids. `None` if nothing (cells or cursor)
/// changed. Single-row diffs get their own compact tags (`c` / `l`).
fn diff_message(a: &Grid, b: &Grid) -> Option<String> {
    let mut rows = grid_rows(a, b);
    if rows.is_empty() && a.cursor == b.cursor {
        return None; // nothing this viewer would see changed
    }
    // Absent = unchanged; Some(None) = became hidden; Some(pos) = moved.
    let cur = (a.cursor != b.cursor).then_some(b.cursor);
    let msg = if rows.len() == 1 {
        match rows.pop().expect("len checked") {
            WireRow::Cell { r, l, cell } => WireMsg::Cell {
                cur,
                r: (r, l, cell.text.clone()),
                style: (!is_plain(cell)).then(|| cell_style(cell)),
            },
            // A whole span sharing one style (or all plain) squashes to the
            // uniform form too: one run string, style flattened once.
            WireRow::Line { r, l, block } if uniform(&block) => {
                let CellBlock {
                    mut text,
                    mut style,
                } = block;
                let Some(Text::Run(run)) = text.pop() else {
                    unreachable!("uniform() guarantees a single run")
                };
                WireMsg::Cell {
                    cur,
                    r: (r, l, run),
                    style: style.pop().map(|(_, _, st)| st),
                }
            }
            row => WireMsg::Line { cur, r: row },
        }
    } else {
        WireMsg::Diff { cur, rows }
    };
    Some(serde_json::to_string(&msg).expect("diff wire message serializes"))
}

/// A shared blank cell (canonical form: a plain space — see `grid_from_screen`),
/// so out-of-range indices (a row that grew/shrank between frames) compare and
/// serialize as an empty cell.
fn blank() -> &'static StyledCell {
    static BLANK: OnceLock<StyledCell> = OnceLock::new();
    BLANK.get_or_init(|| StyledCell {
        text: " ".to_string(),
        ..Default::default()
    })
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

/// Changed lines for the screen: each row's minimal changed cell-index span, as
/// the compact single-cell form when the span is one cell wide.
fn grid_rows<'a>(old: &Grid, new: &'a Grid) -> Vec<WireRow<'a>> {
    old.rows
        .iter()
        .zip(&new.rows)
        .enumerate()
        .filter_map(|(r, (o, n))| {
            let (lo, hi) = row_span(o, n)?;
            let single = (lo == hi)
                .then(|| n.get(lo).unwrap_or(blank()))
                // A bare wire string is one cell per codepoint, so the compact
                // form only fits single-codepoint cells.
                .filter(|c| c.text.chars().nth(1).is_none());
            Some(if let Some(cell) = single {
                WireRow::Cell { r, l: lo, cell }
            } else {
                let cells = (lo..=hi).map(|i| n.get(i).unwrap_or(blank()));
                WireRow::Line {
                    r,
                    l: lo,
                    block: cell_block(cells),
                }
            })
        })
        .collect()
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
        // The row count is implied by `r`; the wire's `h` is for the viewer.
        #[serde(rename = "c", default)]
        cur: Option<(u16, u16)>,
        #[serde(rename = "r")]
        rows: Vec<CellBlockIn>,
    },
    #[serde(rename = "d")]
    Diff {
        #[serde(rename = "c", default, deserialize_with = "double_option")]
        cur: Option<Option<(u16, u16)>>,
        #[serde(rename = "r", default)]
        rows: Vec<WireRowIn>,
    },
    #[serde(rename = "c")]
    Cell {
        #[serde(rename = "c", default, deserialize_with = "double_option")]
        cur: Option<Option<(u16, u16)>>,
        // One cell per codepoint; the flattened style applies to all of them.
        r: (usize, usize, String),
        #[serde(flatten)]
        style: CellStyleIn,
    },
    #[serde(rename = "l")]
    Line {
        #[serde(rename = "c", default, deserialize_with = "double_option")]
        cur: Option<Option<(u16, u16)>>,
        r: WireRowIn,
    },
    #[serde(rename = "b")]
    Banner { html: String },
}

/// Distinguish an absent field from an explicit `null`: with `default`, absent
/// stays `None` while any present value (including null) becomes `Some(inner)`.
fn double_option<'de, T: Deserialize<'de>, D: Deserializer<'de>>(
    d: D,
) -> Result<Option<Option<T>>, D::Error> {
    Deserialize::deserialize(d).map(Some)
}

/// Mirror of [`WireRow`], both forms: `[r, l, entries, runs?]` (line span) and
/// `[r, l, "…", {style}?]` (single cell), dispatched on the third element's type.
/// Either way it lands as a [`CellBlockIn`] so the apply path has one shape.
struct WireRowIn {
    r: usize,
    l: usize,
    block: CellBlockIn,
}

/// The third tuple element: a bare string is one cell, an array is text entries.
enum RowTextIn {
    Single(String),
    Entries(Vec<TextIn>),
}

impl<'de> Deserialize<'de> for RowTextIn {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<RowTextIn, D::Error> {
        struct RowTextVisitor;
        impl<'de> Visitor<'de> for RowTextVisitor {
            type Value = RowTextIn;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a single-cell string or an array of text entries")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<RowTextIn, E> {
                Ok(RowTextIn::Single(v.to_string()))
            }
            fn visit_string<E: de::Error>(self, v: String) -> Result<RowTextIn, E> {
                Ok(RowTextIn::Single(v))
            }
            fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<RowTextIn, A::Error> {
                let mut entries = Vec::new();
                while let Some(t) = seq.next_element()? {
                    entries.push(t);
                }
                Ok(RowTextIn::Entries(entries))
            }
        }
        d.deserialize_any(RowTextVisitor)
    }
}

/// The optional fourth tuple element: a `{style}` object (single-cell form) or an
/// array of `[start, len, style]` runs (line form).
enum RowStyleIn {
    Style(CellStyleIn),
    Runs(Vec<(usize, usize, CellStyleIn)>),
}

impl<'de> Deserialize<'de> for RowStyleIn {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<RowStyleIn, D::Error> {
        struct RowStyleVisitor;
        impl<'de> Visitor<'de> for RowStyleVisitor {
            type Value = RowStyleIn;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a style object or an array of style runs")
            }
            fn visit_map<A: de::MapAccess<'de>>(self, map: A) -> Result<RowStyleIn, A::Error> {
                Ok(RowStyleIn::Style(CellStyleIn::deserialize(
                    de::value::MapAccessDeserializer::new(map),
                )?))
            }
            fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<RowStyleIn, A::Error> {
                let mut runs = Vec::new();
                while let Some(r) = seq.next_element()? {
                    runs.push(r);
                }
                Ok(RowStyleIn::Runs(runs))
            }
        }
        d.deserialize_any(RowStyleVisitor)
    }
}

impl<'de> Deserialize<'de> for WireRowIn {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<WireRowIn, D::Error> {
        struct RowVisitor;
        impl<'de> Visitor<'de> for RowVisitor {
            type Value = WireRowIn;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("[row, left, text, style?]")
            }
            fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<WireRowIn, A::Error> {
                let r = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::missing_field("row"))?;
                let l = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::missing_field("left"))?;
                let text = seq
                    .next_element::<RowTextIn>()?
                    .ok_or_else(|| de::Error::missing_field("text"))?;
                let style = seq.next_element::<RowStyleIn>()?;
                let block = match (text, style) {
                    // A bare string is one cell per codepoint; the single style
                    // (if any) covers all of them.
                    (RowTextIn::Single(s), st) => {
                        let n = s.chars().count();
                        CellBlockIn {
                            text: vec![TextIn::Run(s)],
                            style: match st {
                                Some(RowStyleIn::Style(cs)) => vec![(0, n, cs)],
                                Some(RowStyleIn::Runs(runs)) => runs,
                                None => Vec::new(),
                            },
                        }
                    }
                    (RowTextIn::Entries(text), st) => CellBlockIn {
                        text,
                        style: match st {
                            Some(RowStyleIn::Runs(runs)) => runs,
                            Some(RowStyleIn::Style(cs)) => vec![(0, 1, cs)],
                            None => Vec::new(),
                        },
                    },
                };
                Ok(WireRowIn { r, l, block })
            }
        }
        d.deserialize_seq(RowVisitor)
    }
}

/// Mirror of [`CellBlock`]: positional `[text]` or `[text, style]`.
#[derive(Default)]
struct CellBlockIn {
    text: Vec<TextIn>,
    style: Vec<(usize, usize, CellStyleIn)>,
}

impl<'de> Deserialize<'de> for CellBlockIn {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<CellBlockIn, D::Error> {
        struct BlockVisitor;
        impl<'de> Visitor<'de> for BlockVisitor {
            type Value = CellBlockIn;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("[text, style?]")
            }
            fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<CellBlockIn, A::Error> {
                let text = seq.next_element()?.unwrap_or_default();
                let style = seq.next_element()?.unwrap_or_default();
                Ok(CellBlockIn { text, style })
            }
        }
        d.deserialize_seq(BlockVisitor)
    }
}

/// Mirror of [`Text`]: `0` is a blank cell, a string is one cell per codepoint,
/// `["…"]` is a single multi-codepoint-grapheme cell.
enum TextIn {
    Blank,
    Run(String),
    Cluster(String),
}

impl<'de> Deserialize<'de> for TextIn {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<TextIn, D::Error> {
        struct TextVisitor;
        impl<'de> Visitor<'de> for TextVisitor {
            type Value = TextIn;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("0 (blank), a glyph-run string, or [\"grapheme\"]")
            }
            fn visit_u64<E: de::Error>(self, _: u64) -> Result<TextIn, E> {
                Ok(TextIn::Blank)
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<TextIn, E> {
                Ok(TextIn::Run(v.to_string()))
            }
            fn visit_string<E: de::Error>(self, v: String) -> Result<TextIn, E> {
                Ok(TextIn::Run(v))
            }
            fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<TextIn, A::Error> {
                let g: String = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::missing_field("grapheme"))?;
                Ok(TextIn::Cluster(g))
            }
        }
        d.deserialize_any(TextVisitor)
    }
}

/// Deserialize a flag from `1`/`0` (the wire form) or a bool (leniency).
fn truthy<'de, D: Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    struct FlagVisitor;
    impl Visitor<'_> for FlagVisitor {
        type Value = bool;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("0/1 or a bool")
        }
        fn visit_u64<E: de::Error>(self, v: u64) -> Result<bool, E> {
            Ok(v != 0)
        }
        fn visit_bool<E: de::Error>(self, v: bool) -> Result<bool, E> {
            Ok(v)
        }
    }
    d.deserialize_any(FlagVisitor)
}

/// Mirror of [`CellStyle`]; every field defaults so a sparse entry stays sparse.
#[derive(Deserialize, Default, Clone)]
struct CellStyleIn {
    #[serde(rename = "f", default)]
    fg: Color,
    #[serde(rename = "g", default)]
    bg: Color,
    #[serde(rename = "b", default, deserialize_with = "truthy")]
    bold: bool,
    #[serde(rename = "d", default, deserialize_with = "truthy")]
    dim: bool,
    #[serde(rename = "i", default, deserialize_with = "truthy")]
    italic: bool,
    #[serde(rename = "u", default, deserialize_with = "truthy")]
    underline: bool,
    #[serde(rename = "n", default, deserialize_with = "truthy")]
    inverse: bool,
    #[serde(rename = "w", default, deserialize_with = "truthy")]
    wide: bool,
}

/// Materialize a columnar block into cells (mirror of `viewer.ts::decodeBlock`):
/// expand text runs one cell per codepoint, then overlay the style runs.
fn decode_block(block: CellBlockIn) -> Vec<StyledCell> {
    let mut cells: Vec<StyledCell> = Vec::new();
    for t in block.text {
        match t {
            TextIn::Blank => cells.push(StyledCell::default()),
            TextIn::Cluster(g) => cells.push(StyledCell {
                text: g,
                ..Default::default()
            }),
            TextIn::Run(s) => cells.extend(s.chars().map(|ch| StyledCell {
                text: ch.to_string(),
                ..Default::default()
            })),
        }
    }
    for (start, len, s) in block.style {
        for i in start..start.saturating_add(len) {
            let Some(c) = cells.get_mut(i) else { break };
            c.fg = s.fg;
            c.bg = s.bg;
            c.bold = s.bold;
            c.dim = s.dim;
            c.italic = s.italic;
            c.underline = s.underline;
            c.inverse = s.inverse;
            c.wide = s.wide;
        }
    }
    cells
}

/// Apply a decoded wire message to the previous frame, yielding the new one — the
/// state transition `viewer.ts` performs, mirrored so the hub's matrix stays in
/// lockstep with every browser. `None` = drop the message (a diff arriving while
/// the state is a banner is a desync; forwarding it would corrupt viewers too).
fn apply_wire(prev: &Frame, msg: WireMsgIn) -> Option<Frame> {
    // Normalize the three diff shapes into (cursor, row patches).
    let (cur, rows) = match msg {
        WireMsgIn::Full { w, cur, rows } => {
            return Some(Frame::Screen(Grid {
                cols: w,
                rows: rows.into_iter().map(decode_block).collect(),
                cursor: cur,
            }));
        }
        WireMsgIn::Banner { html } => return Some(Frame::Banner(html)),
        WireMsgIn::Diff { cur, rows } => (cur, rows),
        WireMsgIn::Cell { cur, r, style } => {
            let (row, l, text) = r;
            let n = text.chars().count();
            (
                cur,
                vec![WireRowIn {
                    r: row,
                    l,
                    block: CellBlockIn {
                        text: vec![TextIn::Run(text)],
                        style: vec![(0, n, style)],
                    },
                }],
            )
        }
        WireMsgIn::Line { cur, r } => (cur, vec![r]),
    };
    let Frame::Screen(grid) = prev else {
        return None; // a diff can't apply to a banner — drop it
    };
    let mut grid = grid.clone();
    for patch in rows {
        let Some(row) = grid.rows.get_mut(patch.r) else {
            continue; // out-of-range row: same_layout should prevent this
        };
        for (dx, cell) in decode_block(patch.block).into_iter().enumerate() {
            let i = patch.l + dx;
            if i < row.len() {
                row[i] = cell;
            } else {
                // Mirror viewer.ts: a jagged row can grow — pad with canonical
                // blanks (spaces), then push.
                while row.len() < i {
                    row.push(blank().clone());
                }
                row.push(cell);
            }
        }
    }
    if let Some(c) = cur {
        grid.cursor = c; // absent = unchanged; null = hidden; pos = moved
    }
    Some(Frame::Screen(grid))
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

    /// Wire-cost meter, inert without SG_CORPUS. Replays real terminal recordings
    /// through vt100 with the production 33ms frame coalescing and reports the
    /// exact encoded bytes of the diff stream plus a final full snapshot — the
    /// measurement tool for wire-format changes.
    ///
    /// Record a corpus session (name.tm + name.raw pairs in a directory):
    ///   script -q --log-timing name.tm --log-out name.raw \
    ///     -c 'stty cols 80 rows 24; <the workload>'
    /// Run: SG_CORPUS=<dir> [SG_SIZE=80x24] cargo test zz_measure -- --nocapture
    ///
    /// History: an earlier version of this harness compared rectangle-merge
    /// strategies (identical-span, always-merge, exact-cost greedy, gap-bridging)
    /// and killed rectangles: merging never recovered more than ~1% and usually
    /// lost — hence today's line-only diffs. See commit c8bce9c for the numbers.
    #[test]
    fn zz_measure_wire_cost() {
        let dir = std::env::var("SG_CORPUS").unwrap_or_default();
        if dir.is_empty() {
            return;
        }
        println!(
            "{:<10} {:>7} | {:>11} {:>6} {:>9} | {:>10}",
            "session", "frames", "diff bytes", "rows", "row bytes", "full bytes"
        );
        let (mut td, mut tr, mut trb, mut tf) = (0usize, 0usize, 0usize, 0usize);
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

            let (mut dbytes, mut drows, mut rbytes, mut nframes) = (0usize, 0usize, 0usize, 0usize);
            for pair in frames.windows(2) {
                let (a, b) = (&pair[0], &pair[1]);
                if !same_layout(a, b) {
                    continue; // resize → full frame
                }
                nframes += 1;
                for row in grid_rows(a, b) {
                    drows += 1;
                    // Same accounting as the old rect metric: entry JSON + comma.
                    rbytes += serde_json::to_string(&row).unwrap().len() + 1;
                }
                if let Some(msg) = diff_message(a, b) {
                    dbytes += msg.len();
                }
            }
            let fbytes = full_message_grid(frames.last().unwrap()).len();
            println!(
                "{:<10} {:>7} | {:>11} {:>6} {:>9} | {:>10}",
                name, nframes, dbytes, drows, rbytes, fbytes
            );
            td += dbytes;
            tr += drows;
            trb += rbytes;
            tf += fbytes;
        }
        println!(
            "{:<10} {:>7} | {:>11} {:>6} {:>9} | {:>10}",
            "TOTAL", "", td, tr, trb, tf
        );
    }

    #[test]
    fn one_changed_cell_is_one_minimal_line() {
        let a = grid(&["abc", "def"]);
        let b = grid(&["abc", "dXf"]);
        let rows = grid_rows(&a, &b);
        assert_eq!(rows.len(), 1, "one changed row → one line patch");
        assert_eq!(rows[0].pos(), (1, 1), "span bounds the cell");
        assert_eq!(rows[0].text(), "X");
    }

    #[test]
    fn each_changed_row_is_its_own_line() {
        // Two adjacent changed rows → two line patches, each with its own span
        // (line-only by design; rectangles measured as a net loss).
        let a = grid(&["a..z", "a..z"]);
        let b = grid(&["aQQz", "aWWWWWWWWWWWWWWWWWWWWWWWWWWWWz"]);
        let a = Grid { cols: 30, ..a };
        let rows = grid_rows(&a, &b);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].pos(), (0, 1));
        assert_eq!(rows[0].text(), "QQ");
        assert_eq!(rows[1].pos(), (1, 1));
        assert!(rows[1].text().starts_with("WWWW"));
    }

    #[test]
    fn scattered_rows_stay_separate_lines() {
        // Rows 0 and 2 change (different spans); row 1 unchanged → two patches.
        let a = grid(&["abcd", "efgh", "ijkl"]);
        let b = grid(&["Xbcd", "efgh", "ijYl"]);
        let rows = grid_rows(&a, &b);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].pos(), (0, 0));
        assert_eq!(rows[1].pos(), (2, 2));
    }

    #[test]
    fn wire_shapes_are_positional_and_run_length() {
        // Text merges into strings (one cell per codepoint), styles ride as
        // [start, len, style] runs with 1-flags, lines as [r, l, t, s?] tuples.
        let mut a = grid(&["hello world", "unchanged"]);
        let mut b = grid(&["hellO WOrld", "unchanged"]);
        a.cols = 11;
        b.cols = 11;
        b.rows[0][4].bold = true;
        b.rows[0][5].bold = true;
        b.rows[0][6].bold = true;
        let msg = diff_message(&a, &b).unwrap();
        assert!(
            msg.contains(r#""t":"l""#) && msg.contains(r#""r":[0,4,["O WO"],[[0,3,{"b":1}]]]"#),
            "single-line tag, merged text run, style run with 1-flag: {msg}"
        );
    }

    #[test]
    fn cursor_is_tristate_on_diffs() {
        // Unchanged cursor is omitted entirely; the hub keeps its own.
        let mut a = grid(&["ab", "cd"]);
        let mut b = grid(&["xb", "cd"]);
        a.cursor = Some((1, 1));
        b.cursor = Some((1, 1));
        let msg = diff_message(&a, &b).unwrap();
        assert!(!msg.contains("\"c\":"), "unchanged cursor omitted: {msg}");
        let Frame::Screen(g) = apply(&Frame::Screen(a), &msg).unwrap() else {
            panic!("screen")
        };
        assert_eq!(g.cursor, Some((1, 1)), "hub keeps the cursor when absent");
        // Becoming hidden is an explicit null, applied as None.
        let mut h = b.clone();
        h.cursor = None;
        h.rows[0][0].text = "q".into();
        let msg2 = diff_message(&b, &h).unwrap();
        assert!(msg2.contains("\"c\":null"), "hide is explicit null: {msg2}");
        let Frame::Screen(g2) = apply(&Frame::Screen(b), &msg2).unwrap() else {
            panic!("screen")
        };
        assert_eq!(g2.cursor, None);
    }

    #[test]
    fn single_cell_updates_use_the_compact_form() {
        // A spinner tick: one styled cell → [r, l, "…", {style}] — no entry
        // array, no run framing.
        let mut a = grid(&["x spinner"]);
        let mut b = grid(&["y spinner"]);
        a.cols = 9;
        b.cols = 9;
        b.rows[0][0].fg = Color::Idx(174);
        let msg = diff_message(&a, &b).unwrap();
        assert_eq!(
            msg, r#"{"t":"c","r":[0,0,"y"],"f":174}"#,
            "single-cell tag with flattened style"
        );
        // Plain single cell drops the style element entirely.
        let c = grid(&["z spinner"]);
        let msg2 = diff_message(&b, &c).unwrap();
        assert_eq!(
            msg2, r#"{"t":"c","r":[0,0,"z"]}"#,
            "plain single cell drops the style entirely"
        );
        // Both round-trip through the hub-side decoder.
        let applied = apply(&Frame::Screen(a), &msg).unwrap();
        assert_eq!(applied, Frame::Screen(b.clone()));
        let applied2 = apply(&Frame::Screen(b), &msg2).unwrap();
        assert_eq!(applied2, Frame::Screen(c));
    }

    #[test]
    fn single_cluster_cell_uses_the_entry_form() {
        // A bare wire string is one cell per codepoint everywhere, so a single
        // multi-codepoint grapheme cell must take the ["…"] entry form (t:"l").
        let a = grid(&["ab"]);
        let mut b = grid(&["xb"]);
        b.rows[0][0].text = "e\u{0301}".to_string();
        let msg = diff_message(&a, &b).unwrap();
        assert_eq!(
            msg,
            format!(r#"{{"t":"l","r":[0,0,[["{}"]]]}}"#, "e\u{0301}"),
            "cluster via entries"
        );
        let applied = apply(&Frame::Screen(a), &msg).unwrap();
        assert_eq!(applied, Frame::Screen(b));
    }

    #[test]
    fn uniform_line_squashes_to_the_c_form() {
        // Every changed cell shares one style → multi-char "c" message with the
        // style flattened once.
        let mut a = grid(&["............", "keep"]);
        let mut b = grid(&["INFO started", "keep"]);
        a.cols = 12;
        b.cols = 12;
        for c in &mut b.rows[0] {
            c.fg = Color::Idx(2);
        }
        let msg = diff_message(&a, &b).unwrap();
        assert_eq!(
            msg, r#"{"t":"c","r":[0,0,"INFO started"],"f":2}"#,
            "uniform styled span squashes"
        );
        let applied = apply(&Frame::Screen(a.clone()), &msg).unwrap();
        assert_eq!(applied, Frame::Screen(b.clone()));
        // All-plain spans squash too (no flattened style at all).
        let c = grid(&["hello world.", "keep"]);
        let msg2 = diff_message(&b, &c).unwrap();
        assert_eq!(msg2, r#"{"t":"c","r":[0,0,"hello world."]}"#);
        // Mixed styles stay in the line form.
        let mut d = grid(&["hello WORLD.", "keep"]);
        d.rows[0][6].bold = true;
        let msg3 = diff_message(&c, &d).unwrap();
        assert!(
            msg3.starts_with(r#"{"t":"l""#),
            "mixed span stays a line: {msg3}"
        );
    }

    #[test]
    fn multi_codepoint_grapheme_rides_as_cluster() {
        // In a multi-cell line span, a combining-mark cell must not merge into a
        // run — it gets the ["…"] form.
        let a = grid(&["ab"]);
        let mut b = grid(&["xY"]);
        b.rows[0][0].text = "e\u{0301}".to_string(); // é as e + combining accent
        let msg = diff_message(&a, &b).unwrap();
        assert!(
            msg.contains(r#"["é"]"#) || msg.contains("[\"e\u{301}\"]"),
            "cluster cell wrapped in a one-element array: {msg}"
        );
        // And it round-trips through the hub-side decoder intact.
        let applied = apply(&Frame::Screen(a), &msg).unwrap();
        let Frame::Screen(g) = applied else {
            panic!("screen")
        };
        assert_eq!(g.rows[0][0].text, "e\u{0301}");
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
        assert_eq!(msg, r#"{"t":"d","c":[0,2]}"#, "cursor-only move is minimal");
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
        // Rows are positional blocks: text merged into one run, no style part.
        assert!(full.contains(r#""r":[[["hi"]]]"#), "{full}");
        // Hidden cursor is omitted entirely, not "c":null.
        assert!(!full.contains("\"c\":"), "hidden cursor omitted: {full}");
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
        // One merged text run; one [start, len, style] run for the styled cell.
        assert!(
            full.contains(r#"[["aBc"],[[1,1,{"f":1,"b":1}]]]"#),
            "{full}"
        );
    }

    #[test]
    fn blank_cells_encode_as_zero() {
        // 'a' then a blank (empty-text) cell → the blank rides as 0, not "".
        let mut g = grid(&["a"]);
        g.rows[0].push(StyledCell::default());
        let full = full_message_grid(&g);
        assert!(full.contains(r#"[["a",0]]"#), "blank cell is 0: {full}");
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
    /// leaves explicit blanks — canonical spaces — where the origin simply has a
    /// shorter row: same rendering, different cell count).
    fn assert_grid_equiv(a: &Grid, b: &Grid) {
        let trim = |g: &Grid| {
            let mut g = g.clone();
            for row in &mut g.rows {
                while row.last().is_some_and(|c| c == blank()) {
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

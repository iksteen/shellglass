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
//! There is **no `"t"` tag**: every message type owns one payload key, and the
//! decoder dispatches on which is present. `c` must be tested FIRST — the
//! single-cell form flattens its style letters (`f,g,b,d,i,u,n,w`) into the
//! envelope, so `b`/`d`/`w` there must not be read as banner/full/width.
//!
//! - `{"d":[block,…], w, h, p?}` — a full snapshot (sent to each viewer on
//!   connect, and whenever the screen size changes). `p` = cursor; absent = hidden.
//! - `{"r":[[row, left, text, style?],…], p?}` — changed lines: row index, the
//!   cell index the span starts at, then the block. On diff-family messages `p`
//!   is TRI-STATE (absent = unchanged, null = hidden, [row, col] = moved); a
//!   cursor-only move drops `r` entirely, leaving just `{"p":[row, col]}`.
//! - `{"c":[row, left, "…"], p?, …style}` — a uniform span: one cell per
//!   codepoint, the flattened style applying to every cell. The hottest diffs
//!   (spinner ticks, typing echo, appended log lines) take this form.
//! - `{"l":[row, left, text, style?], p?}` — a single changed line that isn't
//!   uniform (mixed styles, blanks-as-0, or cluster cells).
//! - `{"b": html}` — an error banner.
//! - `{"v": proto, js}` — version hello, first event of every SSE stream: the
//!   wire proto and the baked viewer.js content tag. A page that mismatches on
//!   either reloads itself (see [`PROTO`]).
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
// The SSE endpoint is the one axum-touching corner of this module — HTTP-serving
// builds only (serve/hub); the push client uses just the encode side.
#[cfg(any(feature = "serve", feature = "hub"))]
use axum::response::sse::{Event, KeepAlive, Sse};
#[cfg(any(feature = "serve", feature = "hub"))]
use axum::response::{IntoResponse, Response};
use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
#[cfg(any(feature = "serve", feature = "hub"))]
use std::convert::Infallible;
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::{broadcast, watch};
#[cfg(any(feature = "serve", feature = "hub"))]
use tokio_stream::StreamExt;
#[cfg(any(feature = "serve", feature = "hub"))]
use tokio_stream::wrappers::BroadcastStream;
#[cfg(any(feature = "serve", feature = "hub"))]
use tokio_stream::wrappers::WatchStream;
#[cfg(any(feature = "serve", feature = "hub"))]
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;

use crate::model::{Color, Frame, Grid, ImagePlacement, StyledCell};

/// Viewer wire-protocol version. Injected into the page at serve time
/// (`window.SHELLGLASS.proto`) and announced as the first SSE event on every
/// (re)connect (`{"v":N,…}`): a page whose baked viewer.js predates a
/// server upgrade sees the mismatch on reconnect and reloads itself. Bump on
/// any change to the viewer-facing message format. (The client→hub side is
/// guarded separately by the session-id salt in `proto.rs`.)
pub const PROTO: u32 = 4;

/// The version-hello event data, sent at the head of every SSE stream: the wire
/// proto plus the baked viewer.js content tag — the two things that decide
/// whether a loaded page can keep consuming this stream.
#[cfg(any(feature = "serve", feature = "hub"))]
fn hello_message() -> String {
    format!(
        "{{\"v\":{PROTO},\"js\":\"{}\"}}",
        crate::render::viewer_tag()
    )
}

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
    // Only the SSE connect path reads it; non-serving builds still populate
    // the lock (it's just never asked for).
    #[cfg_attr(not(any(feature = "serve", feature = "hub")), allow(dead_code))]
    full: OnceLock<Arc<str>>,
}

#[cfg(any(feature = "serve", feature = "hub"))]
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
    /// Operator (push source) presence, surfaced to viewers as a named `operator`
    /// SSE event (`1`/`0`) alongside the frame stream — the hub flips it on push
    /// (re)connect and on push disconnect. Standalone leaves it `true`: there the
    /// operator IS the process, and its death drops the SSE stream instead. A `watch`
    /// so a viewer connecting mid-outage reads the current value, not just changes.
    online: watch::Sender<bool>,
}

impl Live {
    /// Create a publisher seeded with `initial` (what a viewer connecting before the
    /// first real frame will see).
    pub fn new(initial: Arc<Frame>) -> Arc<Live> {
        let (diffs, _) = broadcast::channel(BACKLOG);
        let (online, _) = watch::channel(true);
        Arc::new(Live {
            state: ArcSwap::from_pointee(State {
                seq: 0,
                frame: initial,
                full: OnceLock::new(),
            }),
            writer: Mutex::new(()),
            diffs,
            online,
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
    /// connects sound: a delta sent before a viewer subscribes stores its state
    /// before the viewer's snapshot load, so the snapshot's seq covers it and
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

    /// Subscribe to state-change ticks (the SSH viewer's wake-up signal). The payload
    /// is the same `(seq, wire)` the SSE path broadcasts, but the SSH consumer ignores
    /// it and re-reads [`Live::frame`] instead — the broadcast is used purely as "the
    /// screen changed". `Lagged` under a slow client just means "catch up to latest",
    /// which loading the current frame does anyway.
    pub fn ticks(&self) -> broadcast::Receiver<(u64, Arc<str>)> {
        self.diffs.subscribe()
    }

    /// The current frame, lock-free (the same `ArcSwap` snapshot viewers connect
    /// against). Pairs with [`Live::ticks`] for the SSH renderer.
    pub fn frame(&self) -> Arc<Frame> {
        Arc::clone(&self.state.load().frame)
    }

    /// Mark the operator (push source) online/offline. Every connected viewer gets
    /// an `operator` SSE event at once, and viewers connecting later read the current
    /// value. Hub-only in practice (see the `online` field).
    pub fn set_online(&self, on: bool) {
        self.online.send_replace(on);
    }

    /// Whether the operator is currently online (the management API's `live` flag).
    pub fn is_online(&self) -> bool {
        *self.online.borrow()
    }

    /// Subscribe a viewer: an SSE response that emits a full snapshot first, then
    /// each broadcast delta. **Takes no locks** — subscribe first, snapshot second,
    /// and skip deltas the snapshot already covers (seq ≤ snapshot's). On `Lagged`
    /// (viewer overflowed the backlog) it resyncs with a fresh snapshot, raising the
    /// skip threshold — stale retained deltas are discarded exactly, not replayed.
    #[cfg(any(feature = "serve", feature = "hub"))]
    pub fn connect(self: &Arc<Self>) -> Response {
        let rx = self.diffs.subscribe();
        let snap = self.state.load_full();
        let full = snap.full_msg();
        let mut threshold = snap.seq;
        // Version hello first (no id: it isn't a delta and shouldn't move
        // lastEventId), then the full snapshot.
        let hello = tokio_stream::once(Ok::<_, Infallible>(Event::default().data(hello_message())));
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
        // Operator-presence stream, merged in as named `operator` events (1/0). Its
        // own channel (a watch, current value on subscribe) so it's independent of
        // the frame seq/snapshot reconciliation and carries no `id:` (mustn't move
        // lastEventId). A named event → the renderer handles it separately from the
        // wire messages, so this needs no PROTO bump.
        let status = WatchStream::new(self.online.subscribe()).map(|on| {
            Ok::<_, Infallible>(
                Event::default()
                    .event("operator")
                    .data(if on { "1" } else { "0" }),
            )
        });
        let mut resp = Sse::new(hello.chain(head).chain(tail).merge(status))
            .keep_alive(KeepAlive::default())
            .into_response();
        // Tell nginx (and other proxies that honor it) not to buffer this response:
        // its default `proxy_buffering on` batches SSE events and defeats realtime
        // push. Harmless where unrecognized. The stream is never compressed either.
        resp.headers_mut().insert(
            "x-accel-buffering",
            axum::http::HeaderValue::from_static("no"),
        );
        resp
    }
}

/// Encode the delta from `cur` to `next`, or `None` if nothing viewers see changed.
/// Also used by the push client to stream the same deltas to the hub.
pub fn encode_delta(cur: &Frame, next: &Frame) -> Option<Arc<str>> {
    let msg = match (cur, next) {
        (Frame::Banner(old), Frame::Banner(new)) if old == new => return None,
        (_, Frame::Banner(html)) => banner_message(html),
        // An image add/remove/move rides only in the full frame, so any change to
        // the image set forces a full (cheap: images are rare and the set is
        // usually empty ⇒ this compares two empty vecs). Same for the OSC 10/11
        // default-color overrides (`e`, likewise full-frame-only and rare).
        (Frame::Screen(a), Frame::Screen(b))
            if same_layout(a, b)
                && a.images == b.images
                && a.default_colors == b.default_colors =>
        {
            diff_message(a, b)?
        }
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

/// A viewer/hub wire message. Tag-free: each variant serializes to a map keyed by
/// its own payload letter (`d`/`r`/`c`/`l`/`b`), plus an optional cursor `p` and,
/// for [`WireMsg::Cell`], the flattened style. See the module docs for the shapes
/// and the decode-side dispatch order.
enum WireMsg<'a> {
    /// Full snapshot. Cursor is absolute: `Some` = shown at pos, `None` = hidden.
    Full {
        w: u16,
        h: usize,
        cur: Option<(u16, u16)>,
        /// DECSCUSR style, absolute: the `q` key, omitted when 0 (default).
        sty: u8,
        /// OSC 10/11 default fg/bg overrides: the `e` key `[fg, bg]`
        /// (`null` = configured default), omitted when both are default.
        defaults: (Color, Color),
        /// Window title: the `t` key, omitted when empty.
        title: &'a str,
        /// OSC 8 link table (id → URI): the `y` key, omitted when empty.
        links: &'a std::collections::BTreeMap<u32, String>,
        rows: Vec<CellBlock<'a>>,
        /// Inline images (empty for the common text-only case ⇒ `i` key omitted).
        images: &'a [ImagePlacement],
    },
    /// Changed lines. Cursor is TRI-STATE: absent = unchanged, `null` = became
    /// hidden, `[row, col]` = moved. An empty `rows` drops the `r` key (a
    /// cursor-only move serializes to just `{"p":…}`). `sty` (the `q` key) is
    /// two-state: absent = unchanged, value = changed-to (0 = back to default);
    /// the encoder always sends `p` alongside `q`, so old decoders — which
    /// dispatch a rows-less message on `p` — still parse it (as a cursor
    /// no-op) instead of dropping it.
    Diff {
        cur: Option<Option<(u16, u16)>>,
        sty: Option<u8>,
        /// Title, two-state: absent = unchanged, string = changed-to ("" =
        /// cleared). Only on this shape and Full — never the flattened `c`
        /// form, where an old viewer would spread `t` into cell text.
        title: Option<&'a str>,
        /// New OSC 8 table entries this diff's cells introduce (`y`, merged
        /// into the viewer's table). A diff needing this forces this shape.
        links: std::collections::BTreeMap<u32, &'a String>,
        rows: Vec<WireRow<'a>>,
    },
    /// A uniform span — the hottest diffs (spinner ticks, typing echo, appended
    /// log lines) change one line where every cell shares one style: `c` is the
    /// bare `[row, left, "…"]` tuple, the string is ONE CELL PER CODEPOINT, and
    /// the style flattens into the message itself, applying to every cell.
    Cell {
        cur: Option<Option<(u16, u16)>>,
        sty: Option<u8>,
        r: (usize, usize, String),
        style: Option<CellStyle>,
    },
    /// A single changed line: `l` is the bare `[row, left, entries, runs?]` tuple.
    Line {
        cur: Option<Option<(u16, u16)>>,
        sty: Option<u8>,
        r: WireRow<'a>,
    },
    Banner {
        html: &'a str,
    },
}

impl Serialize for WireMsg<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut m = s.serialize_map(None)?;
        match self {
            WireMsg::Full {
                w,
                h,
                cur,
                sty,
                defaults,
                title,
                links,
                rows,
                images,
            } => {
                m.serialize_entry("d", rows)?;
                m.serialize_entry("w", w)?;
                m.serialize_entry("h", h)?;
                if let Some(c) = cur {
                    m.serialize_entry("p", c)?;
                }
                if *sty != 0 {
                    m.serialize_entry("q", sty)?;
                }
                if *defaults != (Color::Default, Color::Default) {
                    m.serialize_entry("e", &[defaults.0, defaults.1])?;
                }
                if !title.is_empty() {
                    m.serialize_entry("t", title)?;
                }
                if !links.is_empty() {
                    m.serialize_entry("y", links)?;
                }
                if !images.is_empty() {
                    m.serialize_entry("i", images)?;
                }
            }
            WireMsg::Diff {
                cur,
                sty,
                title,
                links,
                rows,
            } => {
                if !rows.is_empty() {
                    m.serialize_entry("r", rows)?;
                }
                if let Some(c) = cur {
                    m.serialize_entry("p", c)?;
                }
                if let Some(sty) = sty {
                    m.serialize_entry("q", sty)?;
                }
                if let Some(t) = title {
                    m.serialize_entry("t", t)?;
                }
                if !links.is_empty() {
                    m.serialize_entry("y", links)?;
                }
            }
            WireMsg::Cell { cur, sty, r, style } => {
                m.serialize_entry("c", r)?;
                if let Some(c) = cur {
                    m.serialize_entry("p", c)?;
                }
                if let Some(sty) = sty {
                    m.serialize_entry("q", sty)?;
                }
                if let Some(st) = style {
                    st.flatten_into(&mut m)?;
                }
            }
            WireMsg::Line { cur, sty, r } => {
                m.serialize_entry("l", r)?;
                if let Some(c) = cur {
                    m.serialize_entry("p", c)?;
                }
                if let Some(sty) = sty {
                    m.serialize_entry("q", sty)?;
                }
            }
            WireMsg::Banner { html } => {
                m.serialize_entry("b", html)?;
            }
        }
        m.end()
    }
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
/// object envelope is pure overhead once the shape is fixed.
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
    /// Underline style 1-5 (kitty's `4:n` numbering); `1` doubles as the plain
    /// underline flag, so pre-style decoders (truthiness checks) still render
    /// a single underline for any style — additive in value space, no salt bump.
    #[serde(rename = "u", skip_serializing_if = "is_zero")]
    underline: u8,
    #[serde(
        rename = "s",
        skip_serializing_if = "crate::model::is_false",
        serialize_with = "flag"
    )]
    strike: bool,
    /// Conceal (SGR 8): the glyph is hidden; text still rides the wire (the
    /// buffer keeps it — selection/copy semantics match a real terminal).
    #[serde(
        rename = "o",
        skip_serializing_if = "crate::model::is_false",
        serialize_with = "flag"
    )]
    concealed: bool,
    /// Blink (SGR 5/6): the viewers animate it, like kitty.
    #[serde(
        rename = "x",
        skip_serializing_if = "crate::model::is_false",
        serialize_with = "flag"
    )]
    blink: bool,
    /// Underline color; absent = follow the text color (CSS default).
    #[serde(rename = "k", skip_serializing_if = "crate::model::is_default_color")]
    ulcolor: Color,
    #[serde(
        rename = "n",
        skip_serializing_if = "crate::model::is_false",
        serialize_with = "flag"
    )]
    inverse: bool,
    /// OSC 8 hyperlink id, resolved through the frame's `y` table.
    #[serde(rename = "a", skip_serializing_if = "Option::is_none")]
    link: Option<u32>,
    #[serde(
        rename = "w",
        skip_serializing_if = "crate::model::is_false",
        serialize_with = "flag"
    )]
    wide: bool,
}

impl CellStyle {
    /// Serialize the set attributes as entries in an *existing* map — the flattened
    /// form the single-cell `c` message uses (mirrors the derived map above, merged
    /// into the envelope instead of nested). Same keys, same `1`-flags.
    fn flatten_into<M: serde::ser::SerializeMap>(&self, m: &mut M) -> Result<(), M::Error> {
        if !crate::model::is_default_color(&self.fg) {
            m.serialize_entry("f", &self.fg)?;
        }
        if !crate::model::is_default_color(&self.bg) {
            m.serialize_entry("g", &self.bg)?;
        }
        if !crate::model::is_default_color(&self.ulcolor) {
            m.serialize_entry("k", &self.ulcolor)?;
        }
        if self.underline != 0 {
            m.serialize_entry("u", &self.underline)?;
        }
        if let Some(link) = self.link {
            m.serialize_entry("a", &link)?;
        }
        for (set, key) in [
            (self.bold, "b"),
            (self.dim, "d"),
            (self.italic, "i"),
            (self.strike, "s"),
            (self.concealed, "o"),
            (self.blink, "x"),
            (self.inverse, "n"),
            (self.wide, "w"),
        ] {
            if set {
                m.serialize_entry(key, &1u8)?;
            }
        }
        Ok(())
    }
}

/// A cell with no styling: plain default-colored text (or a blank). Such cells carry
/// only their glyph in the `text` column and never appear in the sparse style map.
fn is_plain(c: &StyledCell) -> bool {
    c.fg == Color::Default
        && c.bg == Color::Default
        && c.ulcolor == Color::Default
        && c.underline == 0
        && c.link.is_none()
        && !(c.bold
            || c.dim
            || c.italic
            || c.strike
            || c.concealed
            || c.blink
            || c.inverse
            || c.wide)
}

fn cell_style(c: &StyledCell) -> CellStyle {
    CellStyle {
        fg: c.fg,
        bg: c.bg,
        bold: c.bold,
        dim: c.dim,
        italic: c.italic,
        underline: c.underline,
        strike: c.strike,
        concealed: c.concealed,
        blink: c.blink,
        ulcolor: c.ulcolor,
        inverse: c.inverse,
        link: c.link,
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
/// `c` form.
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
        sty: g.cursor_style,
        defaults: g.default_colors,
        title: &g.title,
        links: &g.links,
        rows: g.rows.iter().map(|r| cell_block(r.iter())).collect(),
        images: &g.images,
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
    if rows.is_empty()
        && a.cursor == b.cursor
        && a.cursor_style == b.cursor_style
        && a.title == b.title
    {
        return None; // nothing this viewer would see changed
    }
    // Cursor style and title, two-state: absent = unchanged, value =
    // changed-to. When either changes, the cursor rides along even if
    // unmoved: old decoders dispatch a rows-less message on `p`, so `{"q":…}`
    // alone would be dropped by an old hub while `{"p":…,"q":…}` degrades to
    // a harmless cursor no-op.
    let sty = (a.cursor_style != b.cursor_style).then_some(b.cursor_style);
    let title = (a.title != b.title).then_some(b.title.as_str());
    // OSC 8 entries this frame's cells introduce (ids are stable, so an id the
    // previous frame carried needs no re-send; the viewer merges additions and
    // prunes on the next full).
    let links: std::collections::BTreeMap<u32, &String> = b
        .links
        .iter()
        .filter(|(id, _)| !a.links.contains_key(id))
        .map(|(&id, uri)| (id, uri))
        .collect();
    // Absent = unchanged; Some(None) = became hidden; Some(pos) = moved.
    let cur = (a.cursor != b.cursor || sty.is_some() || title.is_some()).then_some(b.cursor);
    // A title change or a new link table entry forces the Diff shape: the
    // compact `c` form flattens its style into the envelope, where old
    // viewers spread unknown keys into cell objects — a `t` there would
    // overwrite cell text (and keeping `y` off the compact forms keeps the
    // decode surface small).
    let msg = if rows.len() == 1 && title.is_none() && links.is_empty() {
        match rows.pop().expect("len checked") {
            WireRow::Cell { r, l, cell } => WireMsg::Cell {
                cur,
                sty,
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
                    sty,
                    r: (r, l, run),
                    style: style.pop().map(|(_, _, st)| st),
                }
            }
            row => WireMsg::Line { cur, sty, r: row },
        }
    } else {
        WireMsg::Diff {
            cur,
            sty,
            title,
            links,
            rows,
        }
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

enum WireMsgIn {
    Full {
        w: u16,
        // The row count is implied by `d`; the wire's `h` is for the viewer.
        cur: Option<(u16, u16)>,
        sty: u8,
        defaults: (Color, Color),
        title: String,
        links: std::collections::BTreeMap<u32, String>,
        rows: Vec<CellBlockIn>,
        images: Vec<ImagePlacement>,
    },
    Diff {
        cur: Option<Option<(u16, u16)>>,
        sty: Option<u8>,
        title: Option<String>,
        links: std::collections::BTreeMap<u32, String>,
        rows: Vec<WireRowIn>,
    },
    Cell {
        cur: Option<Option<(u16, u16)>>,
        sty: Option<u8>,
        // One cell per codepoint; the flattened style applies to all of them.
        r: (usize, usize, String),
        style: CellStyleIn,
    },
    Line {
        cur: Option<Option<(u16, u16)>>,
        sty: Option<u8>,
        r: WireRowIn,
    },
    Banner {
        html: String,
    },
}

/// Deserialize a value out of an already-parsed `Value`, mapping serde_json's error
/// into the caller's deserializer error type.
fn from_val<T: serde::de::DeserializeOwned, E: de::Error>(v: &serde_json::Value) -> Result<T, E> {
    serde_json::from_value(v.clone()).map_err(de::Error::custom)
}

impl<'de> Deserialize<'de> for WireMsgIn {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde_json::Value;
        let v = Value::deserialize(d)?;
        let obj = v
            .as_object()
            .ok_or_else(|| de::Error::custom("wire message must be a JSON object"))?;
        // Tag-free dispatch: each type owns one payload key. `c` FIRST — the
        // single-cell form flattens its style (f,g,b,d,i,u,n,w) into the envelope,
        // so those letters must never reach the banner/full/width branches.
        //
        // Cursor `p` is tri-state on diffs: absent = unchanged, null = hidden.
        let tri = || -> Result<Option<Option<(u16, u16)>>, D::Error> {
            match obj.get("p") {
                None => Ok(None),
                Some(Value::Null) => Ok(Some(None)),
                Some(p) => Ok(Some(Some(from_val(p)?))),
            }
        };
        // Cursor style `q`, two-state on diffs: absent = unchanged.
        let sty = || -> Result<Option<u8>, D::Error> {
            match obj.get("q") {
                None => Ok(None),
                Some(q) => Ok(Some(from_val(q)?)),
            }
        };
        // Window title `t`, two-state on diffs: absent = unchanged.
        let title = || -> Result<Option<String>, D::Error> {
            match obj.get("t") {
                None => Ok(None),
                Some(t) => Ok(Some(from_val(t)?)),
            }
        };
        // OSC 8 link table `y`: full table on fulls, additions on diffs.
        let links = || -> Result<std::collections::BTreeMap<u32, String>, D::Error> {
            match obj.get("y") {
                None => Ok(std::collections::BTreeMap::new()),
                Some(y) => from_val(y),
            }
        };
        if let Some(c) = obj.get("c") {
            return Ok(WireMsgIn::Cell {
                cur: tri()?,
                sty: sty()?,
                r: from_val(c)?,
                style: from_val(&v)?, // whole envelope: reads style letters, ignores c/p/q
            });
        }
        if let Some(l) = obj.get("l") {
            return Ok(WireMsgIn::Line {
                cur: tri()?,
                sty: sty()?,
                r: from_val(l)?,
            });
        }
        if let Some(r) = obj.get("r") {
            return Ok(WireMsgIn::Diff {
                cur: tri()?,
                sty: sty()?,
                title: title()?,
                links: links()?,
                rows: from_val(r)?,
            });
        }
        if let Some(rows) = obj.get("d") {
            let w = obj.get("w").ok_or_else(|| de::Error::missing_field("w"))?;
            // Full cursor is absolute (not tri-state): absent/null = hidden.
            let cur = match obj.get("p") {
                None | Some(Value::Null) => None,
                Some(p) => Some(from_val(p)?),
            };
            return Ok(WireMsgIn::Full {
                w: from_val(w)?,
                cur,
                sty: sty()?.unwrap_or(0), // full is absolute: absent = default
                defaults: match obj.get("e") {
                    Some(e) => {
                        let [f, b]: [Color; 2] = from_val(e)?;
                        (f, b)
                    }
                    None => (Color::Default, Color::Default),
                },
                title: match obj.get("t") {
                    Some(t) => from_val(t)?,
                    None => String::new(),
                },
                links: links()?,
                rows: from_val(rows)?,
                images: match obj.get("i") {
                    Some(i) => from_val(i)?,
                    None => Vec::new(),
                },
            });
        }
        if let Some(b) = obj.get("b") {
            return Ok(WireMsgIn::Banner { html: from_val(b)? });
        }
        if obj.get("p").is_some() || obj.get("q").is_some() || obj.get("t").is_some() {
            // Cursor/title-only change: no row payload, just the tri-state
            // cursor and/or its style/title. (Our encoder always pairs these
            // with p, but accept them bare for robustness.)
            let cur = match obj.get("p") {
                None => None,
                Some(Value::Null) => Some(None),
                Some(p) => Some(Some(from_val(p)?)),
            };
            return Ok(WireMsgIn::Diff {
                cur,
                sty: sty()?,
                title: title()?,
                links: links()?,
                rows: Vec::new(),
            });
        }
        Err(de::Error::custom("wire message has no known payload key"))
    }
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

#[allow(clippy::trivially_copy_pass_by_ref)] // signature required by serde's skip_serializing_if
fn is_zero(v: &u8) -> bool {
    *v == 0
}

/// Deserialize an underline style from its wire number, with the same leniency
/// as [`truthy`]: a bool reads as single/none, an out-of-range number clamps to
/// single — pre-style senders only ever said "underlined".
fn ustyle<'de, D: Deserializer<'de>>(d: D) -> Result<u8, D::Error> {
    struct UnderlineVisitor;
    impl Visitor<'_> for UnderlineVisitor {
        type Value = u8;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("an underline style number or a bool")
        }
        fn visit_u64<E: de::Error>(self, v: u64) -> Result<u8, E> {
            Ok(u8::try_from(v).map_or(1, |n| if n > 5 { 1 } else { n }))
        }
        fn visit_bool<E: de::Error>(self, v: bool) -> Result<u8, E> {
            Ok(u8::from(v))
        }
    }
    d.deserialize_any(UnderlineVisitor)
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
    #[serde(rename = "u", default, deserialize_with = "ustyle")]
    underline: u8,
    #[serde(rename = "s", default, deserialize_with = "truthy")]
    strike: bool,
    #[serde(rename = "o", default, deserialize_with = "truthy")]
    concealed: bool,
    #[serde(rename = "x", default, deserialize_with = "truthy")]
    blink: bool,
    #[serde(rename = "k", default)]
    ulcolor: Color,
    #[serde(rename = "n", default, deserialize_with = "truthy")]
    inverse: bool,
    #[serde(rename = "a", default)]
    link: Option<u32>,
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
            c.strike = s.strike;
            c.concealed = s.concealed;
            c.blink = s.blink;
            c.ulcolor = s.ulcolor;
            c.inverse = s.inverse;
            c.link = s.link;
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
    // Normalize the three diff shapes into (cursor, style, title, links, rows).
    let (cur, sty, title, links, rows) = match msg {
        WireMsgIn::Full {
            w,
            cur,
            sty,
            defaults,
            title,
            links,
            rows,
            images,
        } => {
            return Some(Frame::Screen(Grid {
                cols: w,
                rows: rows.into_iter().map(decode_block).collect(),
                cursor: cur,
                cursor_style: sty,
                default_colors: defaults,
                title,
                links,
                images,
            }));
        }
        WireMsgIn::Banner { html } => return Some(Frame::Banner(html)),
        WireMsgIn::Diff {
            cur,
            sty,
            title,
            links,
            rows,
        } => (cur, sty, title, links, rows),
        WireMsgIn::Cell { cur, sty, r, style } => {
            let (row, l, text) = r;
            let n = text.chars().count();
            (
                cur,
                sty,
                None,
                std::collections::BTreeMap::new(),
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
        WireMsgIn::Line { cur, sty, r } => {
            (cur, sty, None, std::collections::BTreeMap::new(), vec![r])
        }
    };
    let Frame::Screen(grid) = prev else {
        return None; // a diff can't apply to a banner — drop it
    };
    let mut grid = grid.clone();
    // A row is never wider than the screen. Clamp the write column to `cols` so a
    // malformed/malicious diff can't drive the blank-padding growth below into a
    // multi-gigabyte allocation from a tiny message (the wire `l` is an unbounded
    // usize) — and so `patch.l + dx` can't integer-overflow. ponytail: `cols` is
    // itself attacker-set via a full frame, but that's MAX_FRAME-bounded; a per-frame
    // dimension cap is the follow-up.
    let cols = grid.cols as usize;
    for patch in rows {
        let Some(row) = grid.rows.get_mut(patch.r) else {
            continue; // out-of-range row: same_layout should prevent this
        };
        if patch.l >= cols {
            continue; // starts past the screen edge — nothing visible to write
        }
        for (dx, cell) in decode_block(patch.block).into_iter().enumerate() {
            let i = patch.l + dx;
            if i >= cols {
                break; // rest of the run is off-screen
            }
            if i < row.len() {
                row[i] = cell;
            } else {
                // Mirror viewer.ts: a jagged row can grow — pad with canonical
                // blanks (spaces), then push. Bounded by `cols` now.
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
    if let Some(s) = sty {
        grid.cursor_style = s; // absent = unchanged
    }
    if let Some(t) = title {
        grid.title = t; // absent = unchanged
    }
    grid.links.extend(links); // additions merge; fulls replace (above)
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
            cursor_style: 0,
            default_colors: (Color::Default, Color::Default),
            title: String::new(),
            links: std::collections::BTreeMap::new(),
            images: Vec::new(),
        }
    }

    /// Frame-cost meter, inert without SG_FRAMECOST=1 — the measure-first gate
    /// for roadmap item 8 (damage tracking). Times the two per-frame costs the
    /// item targets — `grid_from_screen` (extraction) and `encode_delta`
    /// (comparison) — across screen sizes and workloads, in µs/frame.
    /// Run: SG_FRAMECOST=1 cargo test --release zz_measure_frame_cost -- --nocapture
    #[test]
    fn zz_measure_frame_cost() {
        if std::env::var("SG_FRAMECOST").unwrap_or_default().is_empty() {
            return;
        }
        println!(
            "{:<10} {:<12} | {:>12} {:>12} | {:>10}",
            "size", "workload", "extract µs", "encode µs", "wire B/frm"
        );
        for &(rows, cols) in &[(24u16, 80u16), (50, 200), (100, 320)] {
            for workload in ["typing", "scroll", "churn"] {
                let mut parser = vt100::Parser::new(rows, cols, 0);
                // Fill the screen so extraction/compare see realistic content.
                for r in 0..rows {
                    parser.process(
                        format!("\x1b[{};1H{}", r + 1, "x".repeat(usize::from(cols) - 1))
                            .as_bytes(),
                    );
                }
                parser.process(format!("\x1b[{rows};1H\x1b[K$ ").as_bytes());
                let mut prev = Frame::Screen(crate::parse::grid_from_screen(parser.screen()));
                let frames = 300u32;
                let (mut extract_ns, mut encode_ns, mut bytes) = (0u128, 0u128, 0usize);
                for i in 0..frames {
                    match workload {
                        // One echoed keystroke per frame at the prompt.
                        "typing" => parser.process(if i % 8 == 7 {
                            b"\x1b[K$ " as &[u8]
                        } else {
                            b"a"
                        }),
                        // One appended log line per frame (scrolls the screen).
                        "scroll" => parser.process(
                            format!("\r\n[{i:06}] some log line with detail {i}").as_bytes(),
                        ),
                        // Every row rewritten per frame (cmatrix-class).
                        "churn" => {
                            for r in 0..rows {
                                parser.process(
                                    format!(
                                        "\x1b[{};1H{:>width$}",
                                        r + 1,
                                        i + u32::from(r),
                                        width = usize::from(cols) - 1
                                    )
                                    .as_bytes(),
                                );
                            }
                        }
                        _ => unreachable!(),
                    }
                    let t0 = std::time::Instant::now();
                    let g = crate::parse::grid_from_screen(parser.screen());
                    let t1 = std::time::Instant::now();
                    let next = Frame::Screen(g);
                    let msg = encode_delta(&prev, &next);
                    let t2 = std::time::Instant::now();
                    extract_ns += t1.duration_since(t0).as_nanos();
                    encode_ns += t2.duration_since(t1).as_nanos();
                    bytes += msg.map_or(0, |m| m.len());
                    prev = next;
                }
                println!(
                    "{:<10} {:<12} | {:>12.1} {:>12.1} | {:>10}",
                    format!("{cols}x{rows}"),
                    workload,
                    extract_ns as f64 / 1000.0 / f64::from(frames),
                    encode_ns as f64 / 1000.0 / f64::from(frames),
                    bytes / frames as usize,
                );
            }
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
        assert_eq!(
            msg, r#"{"l":[0,4,["O WO"],[[0,3,{"b":1}]]]}"#,
            "line payload key, merged text run, style run with 1-flag"
        );
    }

    #[test]
    fn version_hello_announces_proto_and_js_tag() {
        let hello = hello_message();
        assert!(hello.starts_with(&format!("{{\"v\":{PROTO},\"js\":\"")));
        assert!(hello.contains(crate::render::viewer_tag()));
        assert_eq!(PROTO, 4, "bump deliberately, with the wire format");
    }

    #[test]
    fn cursor_is_tristate_on_diffs() {
        // Unchanged cursor is omitted entirely; the hub keeps its own.
        let mut a = grid(&["ab", "cd"]);
        let mut b = grid(&["xb", "cd"]);
        a.cursor = Some((1, 1));
        b.cursor = Some((1, 1));
        let msg = diff_message(&a, &b).unwrap();
        assert!(!msg.contains("\"p\":"), "unchanged cursor omitted: {msg}");
        let Frame::Screen(g) = apply(&Frame::Screen(a), &msg).unwrap() else {
            panic!("screen")
        };
        assert_eq!(g.cursor, Some((1, 1)), "hub keeps the cursor when absent");
        // Becoming hidden is an explicit null, applied as None.
        let mut h = b.clone();
        h.cursor = None;
        h.rows[0][0].text = "q".into();
        let msg2 = diff_message(&b, &h).unwrap();
        assert!(msg2.contains("\"p\":null"), "hide is explicit null: {msg2}");
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
            msg, r#"{"c":[0,0,"y"],"f":174}"#,
            "single-cell payload key with flattened style"
        );
        // Plain single cell drops the style element entirely.
        let c = grid(&["z spinner"]);
        let msg2 = diff_message(&b, &c).unwrap();
        assert_eq!(
            msg2, r#"{"c":[0,0,"z"]}"#,
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
        // multi-codepoint grapheme cell must take the ["…"] entry form.
        let a = grid(&["ab"]);
        let mut b = grid(&["xb"]);
        b.rows[0][0].text = "e\u{0301}".to_string();
        let msg = diff_message(&a, &b).unwrap();
        assert_eq!(
            msg,
            format!(r#"{{"l":[0,0,[["{}"]]]}}"#, "e\u{0301}"),
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
            msg, r#"{"c":[0,0,"INFO started"],"f":2}"#,
            "uniform styled span squashes"
        );
        let applied = apply(&Frame::Screen(a.clone()), &msg).unwrap();
        assert_eq!(applied, Frame::Screen(b.clone()));
        // All-plain spans squash too (no flattened style at all).
        let c = grid(&["hello world.", "keep"]);
        let msg2 = diff_message(&b, &c).unwrap();
        assert_eq!(msg2, r#"{"c":[0,0,"hello world."]}"#);
        // Mixed styles stay in the line form.
        let mut d = grid(&["hello WORLD.", "keep"]);
        d.rows[0][6].bold = true;
        let msg3 = diff_message(&c, &d).unwrap();
        assert!(
            msg3.starts_with(r#"{"l":"#),
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
        assert_eq!(msg, r#"{"p":[0,2]}"#, "cursor-only move is minimal");
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
        assert!(full.starts_with("{\"d\":"), "{full}");
        // Rows are positional blocks under `d`: text merged into one run, no style.
        assert!(full.contains(r#""d":[[["hi"]]]"#), "{full}");
        // Hidden cursor is omitted entirely, not "p":null.
        assert!(!full.contains("\"p\":"), "hidden cursor omitted: {full}");
        let banner = banner_message("oops");
        assert_eq!(banner, "{\"b\":\"oops\"}");
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

    // Modern SGR on the wire: `u` carries the style number (1 doubles as the
    // legacy underline flag for pre-style decoders), strikethrough rides `s`,
    // underline color rides `k` — and the hub-side decode reconstructs all
    // three, including the old bare-flag forms.
    #[test]
    fn modern_sgr_rides_the_wire_and_decodes() {
        let mut g = grid(&["xy"]);
        g.rows[0][0].underline = 3; // undercurl
        g.rows[0][0].ulcolor = Color::Idx(1);
        g.rows[0][1].strike = true;
        let full = full_message_grid(&g);
        assert!(full.contains(r#"{"u":3,"k":1}"#), "{full}");
        assert!(full.contains(r#"{"s":1}"#), "{full}");

        // Round trip through the hub's decode path.
        let prev = Frame::Banner("x".into());
        let Some(Frame::Screen(out)) = apply(&prev, &full) else {
            panic!("full applies")
        };
        assert_grid_equiv(&out, &g);

        // Leniency: an old-world sender's `"u":1` (or a bool) is single underline.
        let s: CellStyleIn = serde_json::from_str(r#"{"u":1}"#).unwrap();
        assert_eq!(s.underline, 1);
        let s: CellStyleIn = serde_json::from_str(r#"{"u":true}"#).unwrap();
        assert_eq!(s.underline, 1);
        // An out-of-range style from the future clamps to single, not garbage.
        let s: CellStyleIn = serde_json::from_str(r#"{"u":9}"#).unwrap();
        assert_eq!(s.underline, 1);
    }

    // Conceal rides `o` (additive — old viewers ignore it and show the text,
    // exactly the pre-conceal behavior) and the hub-side decode keeps it. The
    // TEXT still rides the wire: the buffer owns it, only rendering hides it.
    #[test]
    fn conceal_rides_the_wire_and_decodes() {
        let mut g = grid(&["pw"]);
        g.rows[0][0].concealed = true;
        let full = full_message_grid(&g);
        assert!(full.contains(r#"{"o":1}"#), "{full}");
        assert!(
            full.contains("pw"),
            "the concealed glyph still rides: {full}"
        );

        let prev = Frame::Banner("x".into());
        let Some(Frame::Screen(out)) = apply(&prev, &full) else {
            panic!("full applies")
        };
        assert_grid_equiv(&out, &g);
        assert!(out.rows[0][0].concealed && !out.rows[0][1].concealed);
    }

    // Blink rides `x` (additive — old viewers show it steady, the pre-blink
    // behavior) and decodes on the hub side.
    #[test]
    fn blink_rides_the_wire_and_decodes() {
        let mut g = grid(&["bz"]);
        g.rows[0][0].blink = true;
        let full = full_message_grid(&g);
        assert!(full.contains(r#"{"x":1}"#), "{full}");

        let prev = Frame::Banner("x".into());
        let Some(Frame::Screen(out)) = apply(&prev, &full) else {
            panic!("full applies")
        };
        assert_grid_equiv(&out, &g);
        assert!(out.rows[0][0].blink && !out.rows[0][1].blink);
    }

    // DECSCUSR rides as `q`: absolute on fulls (absent = default), two-state on
    // diffs (absent = unchanged) — and always alongside `p`, so an old decoder
    // that dispatches rows-less messages on `p` parses a style-only change as a
    // harmless cursor no-op instead of dropping it.
    #[test]
    fn cursor_style_rides_as_q_with_p_alongside() {
        let mut a = grid(&["hi"]);
        a.cursor = Some((0, 1));
        let mut b = a.clone();
        b.cursor_style = 5; // vim insert: blinking bar
        let msg = diff_message(&a, &b).expect("style change is a change");
        assert_eq!(msg, r#"{"p":[0,1],"q":5}"#);

        // Hub-side apply tracks it; back to default emits q:0 (not absent).
        let Some(Frame::Screen(g)) = apply(&Frame::Screen(a.clone()), &msg) else {
            panic!("applies")
        };
        assert_eq!(g.cursor_style, 5);
        let back = diff_message(&b, &a).expect("style reset is a change");
        assert_eq!(back, r#"{"p":[0,1],"q":0}"#);

        // Fulls carry it absolutely; default is omitted.
        assert!(full_message_grid(&b).contains(r#""q":5"#));
        assert!(!full_message_grid(&a).contains(r#""q""#));
        let prev = Frame::Banner("x".into());
        let Some(Frame::Screen(g)) = apply(&prev, &full_message_grid(&b)) else {
            panic!("full applies")
        };
        assert_eq!(g.cursor_style, 5);
    }

    // OSC 10/11 default-color overrides ride the full frame as `e` (omitted
    // when both default); any change forces a full, and the hub decode
    // reconstructs them for late-join snapshots.
    #[test]
    fn default_colors_ride_full_frames_as_e() {
        let a = grid(&["hi"]);
        let mut b = a.clone();
        b.default_colors = (Color::Default, Color::Rgb(0x30, 0x0a, 0x24));
        let msg = encode_delta(&Frame::Screen(a.clone()), &Frame::Screen(b.clone()))
            .expect("color change is a change");
        assert!(msg.starts_with("{\"d\":"), "override change forces a full");
        assert!(msg.contains(r#""e":[null,[48,10,36]]"#), "{msg}");
        assert!(
            !full_message_grid(&a).contains(r#""e""#),
            "no override ⇒ no key"
        );

        let Some(Frame::Screen(g)) = apply(&Frame::Screen(a.clone()), &msg) else {
            panic!("full applies")
        };
        assert_eq!(g.default_colors, b.default_colors);
        // Resetting back to defaults is also a change (a full with no `e`).
        let msg = encode_delta(&Frame::Screen(b), &Frame::Screen(a)).expect("reset is a change");
        assert!(msg.starts_with("{\"d\":") && !msg.contains(r#""e""#));
    }

    // The window title rides as `t` — on fulls (omitted when empty) and the
    // Diff shape, NEVER the flattened `c` form: an old viewer spreads unknown
    // `c`-envelope keys into cell objects, where `t` would overwrite cell text.
    #[test]
    fn title_rides_full_and_diff_never_the_cell_form() {
        let mut a = grid(&["hi"]);
        a.cursor = Some((0, 0));
        let mut b = a.clone();
        b.title = "vim".into();
        // Title-only change: a rows-less Diff, with `p` riding along for old
        // decoders.
        let msg = diff_message(&a, &b).expect("title change is a change");
        assert_eq!(msg, r#"{"p":[0,0],"t":"vim"}"#);
        let Some(Frame::Screen(g)) = apply(&Frame::Screen(a.clone()), &msg) else {
            panic!("applies")
        };
        assert_eq!(g.title, "vim");

        // Title change + a single-cell change: the compact `c` form is
        // forbidden — the message must be the Diff shape (`r` + `t`).
        let mut c = b.clone();
        c.rows[0][0] = cell('X');
        c.title = "less".into();
        let msg = diff_message(&b, &c).expect("change");
        assert!(
            msg.contains("\"r\":") && msg.contains(r#""t":"less""#) && !msg.contains("\"c\":"),
            "{msg}"
        );

        // Fulls carry it absolutely; empty title ⇒ no key. Clearing back to
        // empty is a change (t:"").
        assert!(full_message_grid(&c).contains(r#""t":"less""#));
        assert!(!full_message_grid(&a).contains("\"t\":"));
        let msg = diff_message(&c, &{
            let mut d = c.clone();
            d.title = String::new();
            d
        })
        .expect("clear is a change");
        assert!(msg.contains(r#""t":"""#), "{msg}");
    }

    // OSC 8: cell ids ride as style key `a`, the id→URI table as `y` — whole
    // table on fulls, new entries only on diffs (which forces the Diff shape,
    // keeping `y` off the compact forms), merged into the hub matrix.
    #[test]
    fn links_ride_as_a_ids_and_y_table() {
        let mut a = grid(&["ab"]);
        a.links.insert(1, "https://one.example".into());
        a.rows[0][0].link = Some(1);
        let full = full_message_grid(&a);
        assert!(full.contains(r#"{"a":1}"#), "{full}");
        assert!(
            full.contains(r#""y":{"1":"https://one.example"}"#),
            "{full}"
        );

        let prev = Frame::Banner("x".into());
        let Some(Frame::Screen(g)) = apply(&prev, &full) else {
            panic!("full applies")
        };
        assert_eq!(g.rows[0][0].link, Some(1));
        assert_eq!(
            g.links.get(&1).map(String::as_str),
            Some("https://one.example")
        );

        // A diff whose cells introduce a NEW id carries just that entry and
        // refuses the compact single-cell form; the hub merges it.
        let mut b = a.clone();
        b.links.insert(2, "https://two.example".into());
        b.rows[0][1].link = Some(2);
        let msg = diff_message(&a, &b).expect("link change is a change");
        assert!(
            msg.contains(r#""y":{"2":"https://two.example"}"#) && !msg.contains(r#""c":"#),
            "{msg}"
        );
        let Some(Frame::Screen(g)) = apply(&Frame::Screen(a.clone()), &msg) else {
            panic!("diff applies")
        };
        assert_eq!(g.rows[0][1].link, Some(2));
        assert_eq!(g.links.len(), 2, "new entry merged, old kept");

        // A diff touching only cells with an already-known id sends no table.
        let mut c = b.clone();
        c.rows[0][0].text = "X".into();
        let msg = diff_message(&b, &c).expect("change");
        assert!(!msg.contains(r#""y":"#), "known id needs no re-send: {msg}");
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
            msg.starts_with("{\"d\":"),
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
    fn hub_decodes_full_frame_images() {
        // A full frame carrying `i` must reconstruct the placements on the hub side,
        // so a late-joining viewer's snapshot shows images (not text only).
        let mut g = grid(&["hi"]);
        g.images.push(ImagePlacement {
            row: 1,
            col: 2,
            cols: Some(3),
            rows: Some(1),
            mime: "image/png".into(),
            data: "AAA".into(),
        });
        let prev = Frame::Banner("x".into());
        let full = encode_delta(&prev, &Frame::Screen(g.clone())).expect("banner → screen is full");
        let Some(Frame::Screen(out)) = apply(&prev, &full) else {
            panic!("full applies")
        };
        assert_eq!(out.images, g.images);
    }

    #[test]
    fn diff_column_clamped_to_screen_width() {
        // A diff whose column index is absurd must not grow the row — otherwise a
        // ~25-byte message could push hundreds of millions of blank cells (DoS).
        let prev = Frame::Screen(grid(&["abc"])); // cols = 3
        let Some(Frame::Screen(g)) = apply(&prev, r#"{"c":[0,500000000,"x"]}"#) else {
            panic!("applies")
        };
        assert_eq!(g.cols, 3);
        assert!(
            g.rows[0].len() <= 3,
            "row not grown past cols: {}",
            g.rows[0].len()
        );
        // A run spilling past the right edge is truncated at `cols`, not grown.
        let Some(Frame::Screen(g2)) = apply(&prev, r#"{"c":[0,2,"XYZ"]}"#) else {
            panic!("applies")
        };
        assert_eq!(g2.rows[0].len(), 3);
        assert_eq!(
            g2.rows[0][2].text, "X",
            "the one in-bounds cell was written"
        );
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
        assert_eq!(
            *live.state.load().frame,
            Frame::Screen(g.clone()),
            "matrix updated"
        );
        let (seq, fwd) = rx.try_recv().expect("full forwarded");
        assert_eq!((seq, &*fwd), (1, full.as_str()), "verbatim bytes, seq 1");

        // A diff advances both the matrix and the seq, still verbatim.
        let g2 = grid(&["hX"]);
        let dmsg = diff_message(&g, &g2).unwrap();
        live.publish_wire(&dmsg);
        assert_eq!(
            *live.state.load().frame,
            Frame::Screen(g2),
            "diff applied to matrix"
        );
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

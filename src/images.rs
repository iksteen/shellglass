//! Inline-image interceptor: pull iTerm2 (OSC 1337) and kitty (APC `_G`) image
//! sequences out of the raw PTY byte stream so they can be forwarded to the browser
//! as `<img>` overlays. vt100 drops these sequences entirely (it implements no
//! DCS/APC/OSC-1337 handler), so the images would otherwise be lost.
//!
//! We only *extract* image sequences; every other byte passes straight through as
//! a [`Step::Passthrough`] to vt100, which is a streaming parser and
//! reassembles partial escape sequences across calls on its own. So the only thing this scanner has to
//! reassemble across PTY read boundaries is an image sequence itself. Like vte and
//! real terminals, an in-flight sequence is *cancelled* by CAN/SUB or an ESC that
//! doesn't form ST (a Ctrl-C'd transfer recovers at the next prompt repaint rather
//! than swallowing the session), and one that outgrows [`MAX_SEQ_BYTES`] is
//! discarded as it streams so it can't grow memory unboundedly.
//!
//! Handled: iTerm2 OSC 1337 `File` — single-shot or multipart
//! (`MultipartFile`/`FilePart`/`FileEnd`); kitty `_G` in
//! PNG (`f=100`) or raw RGB/RGBA (`f=24`/`f=32`, re-encoded to PNG) over the
//! DIRECT (`t=d`) transmission medium only; and sixel DCS (`ESC P … q … ST`,
//! decoded to RGBA and re-encoded to PNG).
//!
//! The kitty file (`t=f`/`t=t`) and shared-memory (`t=s`) mediums carry a
//! filesystem path / shm name in the payload — and this stream is UNTRUSTED. We
//! deliberately DO NOT read them: an injected control sequence (a hostile MOTD,
//! tainted tarball, booby-trapped README) would otherwise exfiltrate any file the
//! broadcaster can read to every viewer. The local terminal still renders those
//! natively; the mirror recovers them because detect-mode clients fall back to
//! direct transmission when we decline the medium (see the tee in `pty.rs`).

use base64::Engine;
use base64::engine::DecodePaddingMode;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig};

/// Standard base64 that *encodes* with canonical `=` padding but *decodes*
/// leniently. Terminal graphics emitters (notably `kitten icat`) send unpadded
/// base64, which the stock `STANDARD` engine rejects — so decode must be
/// padding-indifferent or real images silently fail to parse.
const B64: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    GeneralPurposeConfig::new()
        .with_encode_padding(true)
        .with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

/// One extracted inline image, decoded to raw browser-native file bytes (the
/// form [`crate::proto::content_key`] hashes and the image routes serve).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    /// MIME type sniffed from the decoded bytes (`image/png`, …).
    pub mime: String,
    /// The image file bytes.
    pub bytes: Vec<u8>,
    /// Source pixel dimensions (width, height). Used to derive a cell count when
    /// the app gave none, so the cursor advances below a natural-size image.
    pub px: Option<(u32, u32)>,
    /// How the image is placed on the grid.
    pub place: Placement,
}

/// How an image goes onto the grid: everything the producer knows about the
/// placement that isn't the pixels. The [`Default`] is the plain
/// draw-at-the-cursor placement sixel and iTerm2 make.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Placement {
    /// Display size in terminal cells (cols, rows) if the app specified one;
    /// otherwise the browser renders the image at its natural pixel size.
    pub cells: Option<(u16, u16)>,
    /// The placement declared "don't move the cursor" (kitty `a=p,C=1`): stamp
    /// from the cursor cell downwards without advancing it or scrolling.
    pub hold: bool,
    /// The placement outlives text written over its cells, dying only when they
    /// are erased or scrolled away (kitty's lifetime; see `vt100::PlaceOpts`).
    pub sticky: bool,
    /// The kitty `(image id, placement id)` this came from, when the emitter
    /// names them — the handle an `a=d` deletes the placement by. `None` for
    /// every protocol that just draws at the cursor and never revokes.
    pub key: Option<(u32, u32)>,
    /// Stacking order, when the emitter states one (see
    /// [`crate::model::ImagePlacement::z`]).
    pub z: Option<i32>,
    /// Sub-cell offset in PIXELS (kitty's `X`/`Y`), which the mirror divides by
    /// its cell size like every other pixel dimension — see
    /// [`crate::model::ImagePlacement::x_off`].
    pub off_px: (u16, u16),
}

/// Pixel dimensions from an encoded image's header (any format the browser takes).
fn dims(bytes: &[u8]) -> Option<(u32, u32)> {
    let s = imagesize::blob_size(bytes).ok()?;
    Some((s.width as u32, s.height as u32))
}

/// One unit of `feed` output. The screen thread matches on it to fan the read out
/// to the local terminal, vt100, the mirror, and (for a rejection) back to the
/// app. Each variant names exactly which sinks it touches — there is no implicit
/// "this also goes to the parser" convention. Use [`Step::tee`] for the single
/// question the tee cares about: which bytes (if any) reach the terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Plain bytes → the local terminal AND vt100 (the mirror's parser input).
    Passthrough(Vec<u8>),
    /// Bytes the local terminal renders natively but the mirror ignores and vt100
    /// never sees: a mid-flight transfer chunk, a non-display action, an
    /// oversized-for-the-wire image, an unsupported format.
    TerminalOnly(Vec<u8>),
    /// An inline image: `.0` → the local terminal (which renders it natively), `.1`
    /// is the ready placement stamped into the mirror at the cursor.
    Image(Vec<u8>, Image),
    /// An image whose pixel decode / PNG encode is DEFERRED to a worker thread:
    /// `.0` → the terminal, `.1` is stamped now (dims known up front) and filled
    /// when the worker answers — the screen thread never blocks on a deflate.
    Deferred(Vec<u8>, DeferredImage),
    /// A graphics capability query. The original bytes still reach an attached
    /// terminal; `.1` is a candidate app response used only by a detached owner
    /// that has cached this protocol from its last attached terminal.
    Query(Vec<u8>, Vec<u8>),
    /// Placements the emitter revoked (kitty `a=d`): `.0` → the local terminal,
    /// which drops them itself; the mirror drops the ones `.1` selects, so a
    /// blanked image never lingers on the web.
    Delete(Vec<u8>, DeleteSel),
    /// A kitty file/shm transmission we refuse (its payload is a filesystem path /
    /// shm name and the stream is untrusted — the exfiltration vector). SUPPRESSED
    /// from the local terminal (nothing teed); `.0` is a kitty error echoing the
    /// request id, injected to the APP so a detect-mode client (e.g. `kitten icat`)
    /// falls back to direct transmission — which we DO mirror. Hole closed, image
    /// kept on the web.
    Reject(Vec<u8>),
}

impl Step {
    /// The bytes to write to the local terminal, or `None` for a rejection (which
    /// is suppressed so the terminal never services the refused transmission). The
    /// one place the tee logic lives.
    #[must_use]
    pub fn tee(&self) -> Option<&[u8]> {
        match self {
            Step::Passthrough(b)
            | Step::TerminalOnly(b)
            | Step::Image(b, _)
            | Step::Deferred(b, _)
            | Step::Query(b, _)
            | Step::Delete(b, _) => Some(b),
            Step::Reject(_) => None,
        }
    }
}

/// EXPERIMENTAL: the graphics protocol to transcode sixel INTO, for a terminal
/// that renders kitty/iTerm2 graphics but not sixel (see `pty.rs`'s capability
/// decision). `None` = the terminal does sixel natively, keep the fast raw path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GfxProto {
    Kitty,
    Iterm,
}

/// What a parsed image sequence became, before `push_tee` routes it into a
/// [`Step`]. Internal to the parsers — the screen thread only ever sees `Step`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Segment {
    /// A fully-received inline image, to place at the current cursor cell.
    Image(Image),
    /// An image whose pixel decode / PNG encode is deferred to a worker thread.
    Deferred(DeferredImage),
    /// Rendered by the local terminal but not mirrored (a non-display action, a
    /// mid-flight chunk, an oversized image, an unsupported format).
    Drop,
    /// A refused kitty file/shm transmission — carries the app-bound error response.
    Reject(Vec<u8>),
    /// EXPERIMENTAL sixel transcode: `tee` is the sixel re-encoded in the terminal's
    /// native graphics protocol (kitty/iTerm2), `image` is the PNG for the mirror.
    /// Routes to a `Step::Image` whose tee bytes are the transcode, not the sixel.
    Transcoded { tee: Vec<u8>, image: Image },
    /// A query that draws nothing, with the response a detached virtual terminal
    /// may return from its cached capabilities.
    Query(Vec<u8>),
    /// The emitter revoked placements it made earlier (kitty `a=d`).
    Delete(DeleteSel),
}

/// Which placements an `a=d` revokes. The parser only names them; matching is
/// the mirror's job (`pty::revokes`), because the geometry forms need the
/// placement rects and the cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteSel {
    /// `d=a` — every placement.
    All,
    /// `d=i` (one id, optionally one placement) or `d=r` (an id range).
    Ids {
        first: u32,
        last: u32,
        placement: Option<u32>,
    },
    /// `d=c` — placements covering the cursor's cell.
    Cursor,
    /// `d=p`/`q`/`x`/`y`/`z` — placements covering the stated column and/or row
    /// (1-based on the wire, 0-based here), and/or having the stated z.
    At {
        col: Option<u16>,
        row: Option<u16>,
        z: Option<i32>,
    },
}

/// The stamp-now, decode-later form of an image (see [`Segment::Deferred`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeferredImage {
    pub payload: DeferredPayload,
    /// Pixel dimensions, from cheap metadata (sixel raster attributes, kitty
    /// transmission params) — NOT from decoding.
    pub px: (u32, u32),
    /// Which stream of images this one belongs to; a queued decode is dropped
    /// only when a LATER decode with the same lineage arrives (see
    /// `pty::image_worker`). `0` = the cursor stream — an animation replacing
    /// itself in place, where only the newest frame is worth decoding.
    pub lineage: u64,
    /// How the image is placed on the grid (see [`Image::place`]).
    pub place: Placement,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeferredPayload {
    /// A raw sixel DCS (`ESC P … q … ST`), still to decode + PNG-encode.
    Sixel(Vec<u8>),
    /// A kitty raw framebuffer (RGB/RGBA), still to PNG-encode.
    Raw {
        pixels: Vec<u8>,
        channels: u8,
        w: u32,
        h: u32,
    },
    /// An encoded image placed with a partial crop rectangle `(x, y, w, h)`:
    /// decoding it is the expensive part, so it waits for the worker too.
    Cropped {
        bytes: Vec<u8>,
        crop: (u32, u32, u32, u32),
    },
}

/// The worker half of a deferred image: decode/encode to browser-native PNG
/// bytes (+ actual pixel dims). `fast` picks the cheap deflate level — the
/// worker sets it under backpressure (jobs superseded while this one waited),
/// trading a few percent of size for keeping up with video-rate frames.
pub fn finish_deferred(payload: &DeferredPayload, fast: bool) -> Option<(Vec<u8>, (u32, u32))> {
    match payload {
        DeferredPayload::Sixel(seq) => {
            let img = icy_sixel::SixelImage::decode(seq).ok()?;
            let (w, h) = (
                u32::try_from(img.width).ok()?,
                u32::try_from(img.height).ok()?,
            );
            Some((encode_png(w, h, 4, &img.pixels, fast)?, (w, h)))
        }
        DeferredPayload::Raw {
            pixels,
            channels,
            w,
            h,
        } => Some((encode_png(*w, *h, *channels, pixels, fast)?, (*w, *h))),
        DeferredPayload::Cropped { bytes, crop } => {
            let (pixels, channels, w, h) = decode_png(bytes)?;
            let (x, y, cw, ch) = *crop;
            if x + cw > w || y + ch > h {
                return None; // the rect outgrew the image it was cut from
            }
            let cut = crop_pixels(&pixels, (w, h), *crop, channels);
            Some((encode_png(cw, ch, channels, &cut, fast)?, (cw, ch)))
        }
    }
}

/// Decode an encoded image to 8-bit RGB/RGBA pixels — `(pixels, channels, w, h)`.
/// Only PNG (kitty's `f=100`) and only the two colour types a placement crop
/// needs: anything else is simply not mirrored, which is always allowed. The
/// decode is bounded by [`MAX_IMAGE_BYTES`], so a decompression bomb dies here.
fn decode_png(bytes: &[u8]) -> Option<(Vec<u8>, u8, u32, u32)> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().ok()?;
    if reader.output_buffer_size()? as u64 > MAX_IMAGE_BYTES {
        return None;
    }
    let mut pixels = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut pixels).ok()?;
    let channels = match info.color_type {
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        _ => return None,
    };
    pixels.truncate(info.buffer_size());
    Some((pixels, channels, info.width, info.height))
}

const ITERM: &[u8] = b"\x1b]1337;";
const KITTY: &[u8] = b"\x1b_G";
/// DCS introducer. Sixel is `ESC P <params> q …`; other DCS strings pass through.
const DCS: &[u8] = b"\x1bP";

/// Ceiling on one in-flight image sequence. Generous: the largest legitimate
/// payload (a base64'd image) plus sixel headroom fits well under it. Beyond it
/// the sequence is condemned — its bytes are discarded as they arrive (drain
/// mode) so a runaway or never-terminated transfer can't grow memory unboundedly.
const MAX_SEQ_BYTES: usize = 64 << 20;

/// Ceiling on one decoded image, on *every* transmission path. Inline images ride
/// the full wire frame (diff.rs `i` key), so one image must never inflate a full
/// frame past `proto::MAX_WS_MESSAGE` (64 MiB): the hub closes an oversized
/// message, the client reconnects and re-sends a full frame still containing the
/// same image — a permanent reconnect loop that blinds every viewer until the
/// image scrolls off locally. 16 MiB decoded (≈21.4 MiB as base64) leaves ample
/// headroom for the grid riding alongside.
const MAX_IMAGE_BYTES: u64 = 16 << 20;
/// [`MAX_IMAGE_BYTES`] before base64 decode (4/3 expansion, rounded up).
const MAX_B64_BYTES: usize = (MAX_IMAGE_BYTES as usize).div_ceil(3) * 4;

/// Streaming extractor. Owns only the bytes of an in-flight image sequence plus a
/// tiny carry for a start marker split across a read boundary.
#[derive(Default)]
pub struct Interceptor {
    /// Bytes of a sequence still awaiting its terminator (spans reads).
    seq: Vec<u8>,
    /// `seq` holds an in-flight image sequence for this marker (as opposed to a
    /// split start-marker prefix, which rescans from the top).
    inflight: Option<Marker>,
    /// How far `scan_string` has already searched `seq` for its end — scanning
    /// resumes here on the next read, keeping reassembly linear in the sequence
    /// size instead of rescanning from byte 0 every read.
    scan_from: usize,
    /// The in-flight sequence outgrew [`MAX_SEQ_BYTES`]: its bytes are being
    /// discarded (only a ≤1-byte tail is kept for split-ST detection) and it will
    /// not be parsed on completion.
    overflow: bool,
    /// Concatenated kitty base64 payload across `m=1` chunks, with the display-cell
    /// hint from the first chunk.
    kitty: Option<KittyAccum>,
    /// In-flight iTerm2 multipart transfer (`MultipartFile`→`FilePart`*→`FileEnd`).
    iterm: Option<ItermAccum>,
    /// Transmitted-but-not-yet-placed kitty images, keyed by their `i=`.
    kitty_store: KittyStore,
    /// Intercept kitty / iTerm2 / sixel sequences. A protocol the local terminal
    /// doesn't render is left in the stream (vt100 sees it, matching the terminal)
    /// rather than consumed into a web image the local terminal wouldn't show — see
    /// the capability handshake in `pty.rs`.
    do_kitty: bool,
    do_iterm: bool,
    do_sixel: bool,
    /// EXPERIMENTAL: when set, an intercepted sixel is decoded and re-encoded into
    /// this protocol for the local terminal (which lacks native sixel). `None` =
    /// pass sixel through raw (the fast deferred path for a sixel-native terminal).
    transcode: Option<GfxProto>,
    /// Pixel size of one cell, for deriving the transcoded image's cell footprint
    /// (so kitty/iTerm2 advance the cursor like sixel would). Only used with
    /// `transcode`.
    cell: (u16, u16),
    /// EXPERIMENTAL: monotonic kitty image id for transcoded placements.
    next_id: u32,
    /// EXPERIMENTAL: sixel-bytes → (kitty id, cells, decoded PNG), so tmux re-emitting
    /// the same sixel on every scroll reuses the id (no re-transmit) and the PNG (no
    /// re-decode). Byte-capped.
    sixel_cache: SixelCache,
}

/// EXPERIMENTAL transcode cache entry (see [`Interceptor::sixel_cache`]).
#[derive(Default)]
struct SixelCache {
    map: std::collections::HashMap<u64, CachedSixel>,
    order: std::collections::VecDeque<u64>,
    total: usize,
}

struct CachedSixel {
    id: u32,
    cells: (u16, u16),
    px: (u32, u32),
    png: Vec<u8>,
}

impl SixelCache {
    /// Cap the retained PNGs; a scrolling session usually holds just the one image.
    const CAP: usize = 32 << 20;

    fn get(&self, hash: &u64) -> Option<&CachedSixel> {
        self.map.get(hash)
    }

    fn insert(&mut self, hash: u64, entry: CachedSixel) {
        if self.map.contains_key(&hash) {
            return;
        }
        self.total += entry.png.len();
        self.map.insert(hash, entry);
        self.order.push_back(hash);
        while self.total > Self::CAP && self.order.len() > 1 {
            if let Some(old) = self.order.pop_front()
                && let Some(e) = self.map.remove(&old)
            {
                self.total -= e.png.len();
            }
        }
    }
}

/// Concatenated iTerm2 `FilePart` base64 across a multipart transfer, with the
/// display-cell hint from the opening `MultipartFile`.
struct ItermAccum {
    payload: String,
    cells: Option<(u16, u16)>,
    /// The accumulated payload outgrew [`MAX_B64_BYTES`]: the transfer is
    /// condemned — parts are discarded as they stream and the flush yields
    /// nothing, so an oversized transfer can neither grow memory nor reach the
    /// wire.
    over: bool,
}

struct KittyAccum {
    payload: String,
    /// `a=T` — transmit *and display*. Every other action (`q` capability query,
    /// `t` transmit-only, `d` delete, `f`/`a` animation, `c` compose) draws
    /// nothing at the cursor, so emitting an image for one would show the web
    /// viewer content the local kitty never displayed. The sequence is still
    /// consumed (accumulated and flushed) so `m=1` chains stay coherent. A
    /// transmit-only image isn't lost: it lands in [`KittyStore`] for the `a=p`
    /// that places it later.
    display: bool,
    /// The image id (`i=`) this transfer claims, from the FIRST chunk (only that
    /// one carries the control keys). It is the key a later `a=p` places by.
    id: Option<u32>,
    /// Payload outgrew [`MAX_B64_BYTES`] — condemned, same as `ItermAccum::over`.
    over: bool,
    cells: Option<(u16, u16)>,
    fmt: KittyFmt,
    /// Source pixel dimensions (kitty `s`×`v`), needed to encode raw formats.
    px: Option<(u32, u32)>,
    /// `o=z`: the payload is zlib-compressed.
    zlib: bool,
    /// How the transmitted bytes reach us (the `t` key).
    medium: KittyMedium,
    /// `S`/`O`: byte size/offset of the data within a file/shm object.
    size: Option<usize>,
    offset: usize,
}

/// Images transmitted with an id (`a=t`/`a=T` + `i=`) waiting to be placed by an
/// `a=p`. Byte-capped, oldest-first eviction: an evicted id simply stops
/// mirroring (its later placements show nothing — less than the terminal, which
/// the fidelity rule allows; more would not be).
#[derive(Default)]
struct KittyStore {
    map: std::collections::HashMap<u32, StoredImage>,
    order: std::collections::VecDeque<u32>,
    total: usize,
}

/// A transmitted image, decoded to its post-transmission bytes: a raw
/// framebuffer for `f=24/32` (croppable by memcpy — what zellij sends) or the
/// encoded file for `f=100`.
struct StoredImage {
    data: Vec<u8>,
    fmt: KittyFmt,
    px: (u32, u32),
}

impl KittyStore {
    const CAP: usize = 32 << 20;

    fn get(&self, id: u32) -> Option<&StoredImage> {
        self.map.get(&id)
    }

    /// Store (or REPLACE — an emitter re-transmits the same id, e.g. one scaled
    /// variant per destination size) and evict oldest-first back under the cap.
    fn insert(&mut self, id: u32, image: StoredImage) {
        self.remove(id);
        self.total += image.data.len();
        self.map.insert(id, image);
        self.order.push_back(id);
        while self.total > Self::CAP && self.order.len() > 1 {
            if let Some(old) = self.order.pop_front() {
                self.remove(old);
            }
        }
    }

    /// Every stored id — what `a=d,d=A` frees.
    fn ids(&self) -> Vec<u32> {
        self.order.iter().copied().collect()
    }

    fn remove(&mut self, id: u32) {
        if let Some(old) = self.map.remove(&id) {
            self.total -= old.data.len();
            self.order.retain(|&queued| queued != id);
        }
    }
}

/// The kitty pixel formats we can turn into something the browser renders.
#[derive(Clone, Copy)]
enum KittyFmt {
    /// `f=100`: already an encoded image file — forward as-is.
    Png,
    /// `f=32`: raw RGBA pixels — re-encode as PNG.
    Rgba,
    /// `f=24`: raw RGB pixels — re-encode as PNG.
    Rgb,
    /// Unknown format — drop.
    Unsupported,
}

/// How the transmitted bytes reach us — the kitty `t` key. Only [`Direct`] is
/// honored; the indirect mediums name a filesystem path / shm object we refuse to
/// open (untrusted stream ⇒ arbitrary-file exfiltration). They are still
/// classified so the tee can recognize and reject them (see `pty.rs`).
///
/// [`Direct`]: KittyMedium::Direct
enum KittyMedium {
    /// `t=d` (or absent): the payload is the base64 image itself.
    Direct,
    /// `t=f`/`t=t`: the payload is a base64 filesystem path. NEVER read.
    File,
    /// `t=s`: the payload is a base64 POSIX shared-memory object name. NEVER read.
    Shm,
    /// Unknown transmission medium — drop.
    Unsupported,
}

impl Interceptor {
    /// Intercept all protocols — for tests and when capabilities are unknown.
    #[cfg(test)]
    pub fn new() -> Self {
        Self::with(true, true, true, None, (8, 16))
    }

    /// Intercept only the protocols the terminal supports (per the handshake).
    /// `transcode`/`cell` drive the EXPERIMENTAL sixel→kitty/iTerm2 transcode.
    pub fn with(
        do_kitty: bool,
        do_iterm: bool,
        do_sixel: bool,
        transcode: Option<GfxProto>,
        cell: (u16, u16),
    ) -> Self {
        Self {
            do_kitty,
            do_iterm,
            do_sixel,
            transcode,
            cell,
            ..Default::default()
        }
    }

    /// Change which protocols are intercepted without discarding an in-flight
    /// sequence or multipart transfer. The completed old transfer is therefore
    /// handled coherently; subsequent scans use the new terminal profile.
    pub fn configure(
        &mut self,
        do_kitty: bool,
        do_iterm: bool,
        do_sixel: bool,
        transcode: Option<GfxProto>,
        cell: (u16, u16),
    ) {
        self.do_kitty = do_kitty;
        self.do_iterm = do_iterm;
        self.do_sixel = do_sixel;
        self.transcode = transcode;
        self.cell = cell;
    }

    /// True if `s` is a nonempty proper prefix of an *enabled* start marker — a
    /// marker split across the read boundary, carried until the rest arrives.
    fn split_prefix(&self, s: &[u8]) -> bool {
        !s.is_empty()
            && ((self.do_iterm && s.len() < ITERM.len() && ITERM.starts_with(s))
                || (self.do_kitty && s.len() < KITTY.len() && KITTY.starts_with(s))
                || (self.do_sixel && s.len() < DCS.len() && DCS.starts_with(s)))
    }

    /// Feed one PTY read; returns the ordered [`Step`]s to apply. Bytes of a
    /// sequence still being reassembled are NOT emitted until it completes, so an
    /// image sequence reaches the local terminal one reassembly late (plain text
    /// flushes immediately as passthrough) — the price of being able to suppress a
    /// sequence we reject before the terminal ever sees it.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<Step> {
        let mut out = Vec::new();
        if let Some(marker) = self.inflight {
            // Continue an in-flight sequence: append and resume the end-scan where
            // it left off — never rescanning seen bytes, so reassembly stays
            // linear in the sequence size.
            self.seq.extend_from_slice(chunk);
            match scan_string(&self.seq, marker, self.scan_from) {
                StringEnd::More(m) => {
                    self.scan_from = m;
                    self.enforce_cap(marker);
                }
                StringEnd::End(end) => {
                    let overflowed = std::mem::take(&mut self.overflow);
                    let buf = self.finish_inflight();
                    if overflowed {
                        // Over-cap: dropped from the mirror, but the local terminal
                        // still gets the raw bytes (it renders what it can).
                        push_tee(&mut out, &buf[..end], Segment::Drop);
                        self.cancel_accum(marker);
                    } else {
                        let seg = self.parse_sequence(marker, &buf[..end]);
                        push_tee(&mut out, &buf[..end], seg);
                    }
                    self.scan(&buf[end..], &mut out);
                }
                StringEnd::Abort(resume) => {
                    // The string was cancelled (CAN/SUB, or an ESC that doesn't
                    // form ST). The real terminal consumed and discarded it — and
                    // vt100's own vte state machine would have done the same had
                    // we not intercepted — so drop it and resume at the cancel
                    // point: the bytes that cancelled it (e.g. the prompt repaint
                    // after a Ctrl-C'd transfer) must reach vt100. The cancelled
                    // bytes still tee to the terminal, which saw them too.
                    self.overflow = false;
                    let buf = self.finish_inflight();
                    push_tee(&mut out, &buf[..resume], Segment::Drop);
                    self.cancel_accum(marker);
                    self.scan(&buf[resume..], &mut out);
                }
            }
            return out;
        }
        if self.seq.is_empty() {
            self.scan(chunk, &mut out);
        } else {
            // A start marker split across the read boundary — prepend the carried
            // prefix (a handful of bytes) and rescan from the top.
            let mut data = std::mem::take(&mut self.seq);
            data.extend_from_slice(chunk);
            self.scan(&data, &mut out);
        }
        out
    }

    /// Scan plain bytes for image-sequence markers; non-image bytes become
    /// passthrough [`Step`]s (`None` action). An unterminated sequence or split
    /// marker is carried in `self.seq` for the next read (and NOT teed until it
    /// completes, so a rejected one can be suppressed).
    fn scan(&mut self, data: &[u8], out: &mut Vec<Step>) {
        let mut pass_start = 0usize;
        let mut i = 0usize;

        while i < data.len() {
            // Fast path: only ESC can begin something we care about.
            if data[i] != 0x1b {
                i += 1;
                continue;
            }
            let rest = &data[i..];
            let marker = if self.do_iterm && rest.starts_with(ITERM) {
                Marker::Iterm
            } else if self.do_kitty && rest.starts_with(KITTY) {
                Marker::Kitty
            } else if self.do_sixel && rest.starts_with(DCS) {
                match sixel_dcs(rest) {
                    Some(true) => Marker::Sixel,
                    Some(false) => {
                        i += 1; // a non-sixel DCS — leave it for vt100
                        continue;
                    }
                    None => {
                        // DCS header split across the read boundary — carry the tail.
                        push_pass(out, &data[pass_start..i]);
                        self.seq = rest.to_vec();
                        return;
                    }
                }
            } else if self.split_prefix(rest) {
                // Possible marker split across the read boundary — carry the tail.
                push_pass(out, &data[pass_start..i]);
                self.seq = rest.to_vec();
                return;
            } else {
                i += 1;
                continue;
            };

            // Find the sequence's end (terminator or cancellation) after the marker.
            match scan_string(rest, marker, 1) {
                StringEnd::End(end) => {
                    // Whole sequence is `rest[..end]`.
                    push_pass(out, &data[pass_start..i]);
                    let seg = self.parse_sequence(marker, &rest[..end]);
                    push_tee(out, &rest[..end], seg);
                    i += end;
                    pass_start = i;
                }
                StringEnd::Abort(resume) => {
                    // Cancelled string — dropped from the mirror but still teed to
                    // the terminal (which saw and discarded it); resume at the
                    // cancel point.
                    push_pass(out, &data[pass_start..i]);
                    push_tee(out, &rest[..resume], Segment::Drop);
                    self.cancel_accum(marker);
                    i += resume;
                    pass_start = i;
                }
                StringEnd::More(m) => {
                    // End not in this read — carry the partial sequence.
                    push_pass(out, &data[pass_start..i]);
                    self.seq = rest.to_vec();
                    self.inflight = Some(marker);
                    self.scan_from = m;
                    self.enforce_cap(marker);
                    return;
                }
            }
        }
        push_pass(out, &data[pass_start..]);
    }

    /// Condemn an in-flight sequence that outgrew [`MAX_SEQ_BYTES`]: drop the
    /// already-scanned bytes (keeping only the unscanned tail — at most a trailing
    /// ESC awaiting its `\`) and flag it so completion parses nothing. Repeated
    /// every read, this bounds memory to roughly one PTY read regardless of how
    /// long the sequence runs.
    fn enforce_cap(&mut self, marker: Marker) {
        if self.overflow || self.seq.len() > MAX_SEQ_BYTES {
            self.seq.drain(..self.scan_from);
            self.scan_from = 0;
            self.overflow = true;
            self.cancel_accum(marker);
        }
    }

    /// Leave the in-flight state, taking the buffered sequence.
    fn finish_inflight(&mut self) -> Vec<u8> {
        self.inflight = None;
        self.scan_from = 0;
        std::mem::take(&mut self.seq)
    }

    /// A cancelled/condemned sequence breaks any multi-sequence transfer it was
    /// part of — drop the matching accumulator so a later transfer can't inherit
    /// half a payload.
    fn cancel_accum(&mut self, marker: Marker) {
        match marker {
            Marker::Kitty => self.kitty = None,
            Marker::Iterm => self.iterm = None,
            Marker::Sixel => {}
        }
    }

    /// Bytes currently buffered for a carried sequence — tests assert drain mode
    /// keeps this bounded.
    #[cfg(test)]
    fn carried_len(&self) -> usize {
        self.seq.len()
    }

    /// Parse a complete image sequence (marker .. terminator inclusive). Every
    /// sequence maps to a [`Segment`]: an image, a [`Segment::Reject`] (refused
    /// file/shm), or [`Segment::Drop`] (rendered by the local terminal but not
    /// mirrored — a mid-flight chunk, a non-display action, an over-cap image).
    fn parse_sequence(&mut self, marker: Marker, seq: &[u8]) -> Segment {
        match marker {
            Marker::Iterm => self.parse_iterm(seq),
            Marker::Kitty => self.parse_kitty(seq),
            Marker::Sixel => self.parse_sixel(seq),
        }
    }

    /// A sixel DCS. Natively it's DEFERRED (decode on a worker, the video path).
    /// EXPERIMENTAL: when `transcode` is set (terminal has kitty/iTerm2 but not
    /// sixel), it is instead decoded now and re-encoded for that protocol.
    fn parse_sixel(&mut self, seq: &[u8]) -> Segment {
        match self.transcode {
            Some(proto) => self.sixel_transcoded(seq, proto).unwrap_or(Segment::Drop),
            None => sixel_image(seq).unwrap_or(Segment::Drop),
        }
    }

    /// EXPERIMENTAL: decode a sixel and re-encode it for a non-sixel terminal.
    /// Kitty uses UNICODE PLACEHOLDERS (a virtual placement + placeholder text
    /// cells) so the image scrolls with content and its lifecycle is managed by the
    /// grid — no ghosts on redraw, and it survives tmux; iTerm2 inline images flow
    /// with text on their own. A per-session cache keyed on the sixel bytes reuses
    /// the kitty image id (skip re-transmit) and the decoded PNG (skip re-decode)
    /// when tmux re-emits the same sixel on every scroll. Synchronous (no worker) —
    /// this path only runs on non-sixel terminals; the fast route is untouched.
    fn sixel_transcoded(&mut self, seq: &[u8], proto: GfxProto) -> Option<Segment> {
        let hash = hash_bytes(seq);
        let (id, cells, px, png, fresh) = match self.sixel_cache.get(&hash) {
            Some(c) => (c.id, c.cells, c.px, c.png.clone(), false),
            None => {
                let (png, px) = finish_deferred(&DeferredPayload::Sixel(seq.to_vec()), false)?;
                if png.len() as u64 > MAX_IMAGE_BYTES {
                    return None;
                }
                let cells = cells_for(px.0, px.1, self.cell);
                self.next_id = self.next_id.wrapping_add(1).max(1);
                let id = self.next_id & 0x00ff_ffff; // 24-bit: encodable in an fg color
                self.sixel_cache.insert(
                    hash,
                    CachedSixel {
                        id,
                        cells,
                        px,
                        png: png.clone(),
                    },
                );
                (id, cells, px, png, true)
            }
        };
        let tee = match proto {
            // Only transmit the image data on first sight; re-emissions reuse the id.
            GfxProto::Kitty => kitty_placeholder(id, cells, fresh.then_some(png.as_slice())),
            GfxProto::Iterm => iterm_encode(&png, cells),
        };
        Some(Segment::Transcoded {
            tee,
            // Mirror as if native sixel (cells=None → natural size from px), so the
            // web looks the same whether or not the terminal needed the transcode.
            image: Image {
                mime: "image/png".to_string(),
                bytes: png,
                px: Some(px),
                place: Placement::default(),
            },
        })
    }

    /// Handle one iTerm2 `\x1b]1337;` sequence: a single-shot `File=` image, or one
    /// verb of a multipart transfer (`MultipartFile=` opens, `FilePart=` appends,
    /// `FileEnd` flushes). A verb that yields no image is [`Segment::Drop`] — the
    /// local terminal still processes it (renders, accumulates, or saves a download).
    fn parse_iterm(&mut self, seq: &[u8]) -> Segment {
        let body = strip_terminator(&seq[ITERM.len()..]);
        if let Some(args) = body.strip_prefix(b"File=") {
            return iterm_single(args).map_or(Segment::Drop, Segment::Image);
        }
        if let Some(args) = body.strip_prefix(b"MultipartFile=") {
            let kv = parse_kv(args, b';');
            // `inline=0` (the default) is a *download* — iTerm2 saves the file and
            // renders nothing, so neither do we. No accumulator ⇒ the transfer's
            // FilePart/FileEnd verbs fall through as no-ops.
            if kv.get("inline").map(String::as_str) == Some("1") {
                self.iterm = Some(ItermAccum {
                    payload: String::new(),
                    cells: cells_from(kv.get("width"), kv.get("height")),
                    over: false,
                });
            }
            return Segment::Drop;
        }
        if let Some(part) = body.strip_prefix(b"FilePart=") {
            if let Some(acc) = self.iterm.as_mut() {
                let part = std::str::from_utf8(part).unwrap_or("").trim();
                // Condemn an over-cap transfer as it streams — don't buffer what
                // the wire could never carry.
                if acc.over || acc.payload.len() + part.len() > MAX_B64_BYTES {
                    acc.over = true;
                    acc.payload = String::new();
                } else {
                    acc.payload.push_str(part);
                }
            }
            return Segment::Drop;
        }
        if body.starts_with(b"FileEnd") {
            if let Some(acc) = self.iterm.take()
                && !acc.over
                && let Some(img) = image_from_b64(acc.payload, acc.cells)
            {
                return Segment::Image(img);
            }
            return Segment::Drop;
        }
        Segment::Drop
    }

    /// Handle one kitty `_G` sequence, accumulating `m=1` chunks.
    fn parse_kitty(&mut self, seq: &[u8]) -> Segment {
        // Strip `\x1b_G` prefix and `\x1b\\` suffix.
        let body = &seq[KITTY.len()..seq.len().saturating_sub(2)];
        let (control, payload) = match body.iter().position(|&b| b == b';') {
            Some(p) => (&body[..p], &body[p + 1..]),
            None => (body, &b""[..]),
        };
        let ctrl = parse_kv(control, b',');
        let more = ctrl.get("m").map(|v| v == "1").unwrap_or(false);

        // A direct capability query is safe to answer from a cached terminal
        // profile while detached. File/shm queries retain the security fallback:
        // reject them so a detect-mode client retries with direct transmission.
        if ctrl.get("a").map(String::as_str) == Some("q") {
            return match ctrl.get("t").map(String::as_str) {
                Some("f" | "t" | "s") => Segment::Reject(kitty_reject(&ctrl)),
                None | Some("d") => Segment::Query(kitty_reply(&ctrl, "OK")),
                _ => Segment::Drop,
            };
        }

        // `a=p` places an image transmitted earlier — no payload, no accumulator.
        if ctrl.get("a").map(String::as_str) == Some("p") {
            return self.kitty_place(&ctrl);
        }
        if ctrl.get("a").map(String::as_str) == Some("d") {
            return self.kitty_delete(&ctrl);
        }

        // Start an accumulator on the first chunk of a transfer — the one that
        // carries the format/medium/size control keys (continuation chunks only
        // repeat `m`).
        if self.kitty.is_none() {
            let fmt = match ctrl.get("f").map(String::as_str) {
                Some("100") => KittyFmt::Png,
                Some("32") | None => KittyFmt::Rgba, // kitty's default format is 32
                Some("24") => KittyFmt::Rgb,
                _ => KittyFmt::Unsupported,
            };
            let medium = match ctrl.get("t").map(String::as_str) {
                None | Some("d") => KittyMedium::Direct, // default d=direct
                Some("f" | "t") => KittyMedium::File,
                Some("s") => KittyMedium::Shm,
                _ => KittyMedium::Unsupported,
            };
            self.kitty = Some(KittyAccum {
                payload: String::new(),
                // kitty's default action is `t` (transmit-only) — absent ⇒ no display.
                display: ctrl.get("a").map(String::as_str) == Some("T"),
                id: ctrl.get("i").and_then(|v| v.parse().ok()),
                over: false,
                cells: cells_from(ctrl.get("c"), ctrl.get("r")),
                fmt,
                px: cells_from(ctrl.get("s"), ctrl.get("v"))
                    .map(|(w, h)| (u32::from(w), u32::from(h))),
                zlib: ctrl.get("o").map(|o| o == "z").unwrap_or(false),
                medium,
                size: ctrl.get("S").and_then(|v| v.parse().ok()),
                offset: ctrl.get("O").and_then(|v| v.parse().ok()).unwrap_or(0),
            });
        }
        if let Some(acc) = self.kitty.as_mut() {
            let payload = std::str::from_utf8(payload).unwrap_or("");
            // Condemn an over-cap transfer as it streams (see ItermAccum::over).
            if acc.over || acc.payload.len() + payload.len() > MAX_B64_BYTES {
                acc.over = true;
                acc.payload = String::new();
            } else {
                acc.payload.push_str(payload);
            }
        }
        if more {
            // Mid-flight chunk: the local terminal reassembles it; we wait for the
            // rest. Its bytes still tee to the terminal (Drop), nothing stamped yet.
            return Segment::Drop;
        }
        let Some(acc) = self.kitty.take() else {
            return Segment::Drop;
        };
        // SECURITY / fallback: the file/shm mediums carry a filesystem path / shm
        // name, and the stream is UNTRUSTED (an injected sequence — hostile MOTD,
        // tainted tarball, booby-trapped README). Reading it would exfiltrate any
        // file the broadcaster can read (raw framebuffer needs no image header, and
        // S/O pages a big file across sequences). We never open a path from the
        // stream — instead we REJECT the transmission: suppress it from the local
        // terminal and answer the app with a kitty error, so a detect-mode client
        // (kitten icat) concludes the medium is unsupported and resends via direct
        // transmission, which we DO mirror. See `kitty_reject` for the response.
        if matches!(acc.medium, KittyMedium::File | KittyMedium::Shm) {
            return Segment::Reject(kitty_reject(&ctrl));
        }
        // An over-cap image or an unsupported/non-direct medium is rendered (or
        // ignored) by the local terminal but not mirrored.
        if acc.over || !matches!(acc.medium, KittyMedium::Direct) {
            return Segment::Drop;
        }
        let Some(bytes) = kitty_payload(&acc) else {
            return Segment::Drop;
        };
        // Keep the image for a later `a=p`; `s`/`v` carry the dims of a raw
        // framebuffer, an encoded file carries its own.
        let stored = acc.id.zip(acc.px.or_else(|| dims(&bytes)));
        if !acc.display {
            // Transmit-only: nothing is drawn now, so nothing is mirrored now.
            if let Some((id, px)) = stored {
                self.kitty_store.insert(
                    id,
                    StoredImage {
                        data: bytes,
                        fmt: acc.fmt,
                        px,
                    },
                );
            }
            return Segment::Drop;
        }
        if let Some((id, px)) = stored {
            self.kitty_store.insert(
                id,
                StoredImage {
                    data: bytes.clone(),
                    fmt: acc.fmt,
                    px,
                },
            );
        }
        kitty_segment(
            bytes,
            acc.fmt,
            acc.px,
            0,
            Placement {
                cells: acc.cells,
                // Same kitty lifetime as `a=p` (see `kitty_place`): a print into
                // the image's cells doesn't erase it.
                sticky: true,
                ..Placement::default()
            },
        )
        .unwrap_or(Segment::Drop)
    }

    /// Revoke placements (`a=d`). The uppercase form of a selector also frees
    /// the transmitted image, which we can only do where the id is named
    /// (`I`/`A`/`R`) — the geometry forms would need a placement→image map, and
    /// the store is byte-capped anyway, so an unfreed image just ages out.
    ///
    /// Two kitty selectors are deliberately not honored:
    /// - `d=f` deletes animation *frames*, which no placement depends on.
    /// - `d=n` selects by image NUMBER (`I=`), which the terminal maps to an id
    ///   in a reply we never see. We therefore never mirror a placement made by
    ///   number either, so there is nothing stale for it to leave behind.
    fn kitty_delete(&mut self, ctrl: &std::collections::HashMap<String, String>) -> Segment {
        let what = ctrl.get("d").map_or("a", String::as_str);
        let num = |k: &str| ctrl.get(k).and_then(|v| v.parse::<u32>().ok());
        // Cell coordinates are 1-based on the wire, like CUP's.
        let cell = |k: &str| num(k).and_then(|v| u16::try_from(v.saturating_sub(1)).ok());
        let z = ctrl.get("z").and_then(|v| v.parse::<i32>().ok());
        let ids = match what {
            "i" | "I" => num("i").map(|id| (id, id)),
            "r" | "R" => num("x").zip(num("y")),
            _ => None,
        };
        if what.starts_with(['I', 'A', 'R']) {
            let free: Vec<u32> = match ids {
                Some((first, last)) => (first..=last).collect(),
                // `d=A` frees every image this emitter transmitted.
                None if what == "A" => self.kitty_store.ids(),
                None => Vec::new(),
            };
            for id in free {
                self.kitty_store.remove(id);
            }
        }
        match what {
            "i" | "I" | "r" | "R" => match ids {
                Some((first, last)) => Segment::Delete(DeleteSel::Ids {
                    first,
                    last,
                    placement: num("p"),
                }),
                None => Segment::Drop,
            },
            "a" | "A" => Segment::Delete(DeleteSel::All),
            "c" | "C" => Segment::Delete(DeleteSel::Cursor),
            "p" | "P" => Segment::Delete(DeleteSel::At {
                col: cell("x"),
                row: cell("y"),
                z: None,
            }),
            "q" | "Q" => Segment::Delete(DeleteSel::At {
                col: cell("x"),
                row: cell("y"),
                z,
            }),
            "x" | "X" => Segment::Delete(DeleteSel::At {
                col: cell("x"),
                row: None,
                z: None,
            }),
            "y" | "Y" => Segment::Delete(DeleteSel::At {
                col: None,
                row: cell("y"),
                z: None,
            }),
            "z" | "Z" => Segment::Delete(DeleteSel::At {
                col: None,
                row: None,
                z,
            }),
            _ => Segment::Drop,
        }
    }

    /// Place an image transmitted earlier (`a=p`). The emitter positions the
    /// cursor itself and hands us a SOURCE-PIXEL crop rectangle (`x,y,w,h`) —
    /// pre-scaled to the destination, so the crop's pixel size IS the display
    /// size and the usual `px / cell` derivation gives the cell footprint.
    fn kitty_place(&self, ctrl: &std::collections::HashMap<String, String>) -> Segment {
        let num = |k: &str| ctrl.get(k).and_then(|v| v.parse::<u32>().ok());
        // An id we never saw (evicted, or transmitted before we started
        // watching) simply isn't mirrored — the tee still draws it locally.
        let Some(id) = num("i") else {
            return Segment::Drop;
        };
        let Some(img) = self.kitty_store.get(id) else {
            return Segment::Drop;
        };
        let (iw, ih) = img.px;
        let (x, y) = (num("x").unwrap_or(0).min(iw), num("y").unwrap_or(0).min(ih));
        let (w, h) = (
            num("w").unwrap_or(iw - x).min(iw - x),
            num("h").unwrap_or(ih - y).min(ih - y),
        );
        if w == 0 || h == 0 {
            return Segment::Drop;
        }
        let whole = (x, y, w, h) == (0, 0, iw, ih);
        let placement = num("p").unwrap_or(0);
        let place = Placement {
            cells: cells_from(ctrl.get("c"), ctrl.get("r")),
            hold: ctrl.get("C").map(String::as_str) == Some("1"),
            // kitty keeps a placement until it is deleted; text drawn over it
            // doesn't erase it the way it erases a sixel.
            sticky: true,
            key: Some((id, placement)),
            // Only what the emitter states: a placement with no `z=` keeps the
            // mirror's legacy under-the-text painting. kitty's own default
            // (z=0, over the text) would bury the shell prompt our cursor model
            // deliberately leaves on an image's last row.
            z: ctrl.get("z").and_then(|v| v.parse().ok()),
            // `X`/`Y` position the image inside its cell — how an emitter tiles
            // one image across several placements without seams.
            off_px: (
                u16::try_from(num("X").unwrap_or(0)).unwrap_or(0),
                u16::try_from(num("Y").unwrap_or(0)).unwrap_or(0),
            ),
        };
        let lineage = hash_bytes(&[id.to_le_bytes(), placement.to_le_bytes()].concat());
        match img.fmt {
            KittyFmt::Rgba | KittyFmt::Rgb => {
                let channels = if matches!(img.fmt, KittyFmt::Rgba) {
                    4
                } else {
                    3
                };
                let bytes = crop_pixels(&img.data, (iw, ih), (x, y, w, h), channels);
                kitty_segment(bytes, img.fmt, Some((w, h)), lineage, place)
            }
            // The whole image passes through undecoded; a partial crop has to be
            // decoded, cut and re-encoded, which the worker does off the screen
            // thread (mirroring it UNCROPPED would paint over what the terminal
            // clipped away — more than the local screen shows, which the mirror
            // never does).
            KittyFmt::Png if whole => {
                kitty_segment(img.data.clone(), img.fmt, Some((w, h)), lineage, place)
            }
            KittyFmt::Png => Some(Segment::Deferred(DeferredImage {
                payload: DeferredPayload::Cropped {
                    bytes: img.data.clone(),
                    crop: (x, y, w, h),
                },
                px: (w, h),
                lineage,
                place,
            })),
            KittyFmt::Unsupported => None,
        }
        .unwrap_or(Segment::Drop)
    }
}

/// Cut a `(x, y, w, h)` pixel rectangle out of a raw `channels`-byte-per-pixel
/// framebuffer. A row-by-row copy; the whole-image case just clones.
fn crop_pixels(
    data: &[u8],
    (iw, _ih): (u32, u32),
    (x, y, w, h): (u32, u32, u32, u32),
    channels: u8,
) -> Vec<u8> {
    let px = usize::from(channels);
    let stride = iw as usize * px;
    let row = w as usize * px;
    let mut out = Vec::with_capacity(row * h as usize);
    for line in 0..h as usize {
        let start = (y as usize + line) * stride + x as usize * px;
        match data.get(start..start + row) {
            Some(slice) => out.extend_from_slice(slice),
            // A short/truncated framebuffer: pad so the PNG encode still matches
            // the declared dims instead of failing the whole placement.
            None => out.resize(row * (line + 1), 0),
        }
    }
    out
}

/// The bytes a completed kitty DIRECT transmission carries, after base64, the
/// `S`/`O` window and `o=z` inflation — `None` on any malformed or over-cap
/// payload (the caller renders that as [`Segment::Drop`]).
fn kitty_payload(acc: &KittyAccum) -> Option<Vec<u8>> {
    if matches!(acc.fmt, KittyFmt::Unsupported) {
        return None;
    }
    // Direct: the base64 payload is the image itself.
    let mut bytes = B64.decode(acc.payload.trim()).ok()?;
    // `S`/`O`: the transmitted data may be a window into the payload.
    if let Some(sz) = acc.size {
        bytes = bytes.get(acc.offset..acc.offset.checked_add(sz)?)?.to_vec();
    }
    if acc.zlib {
        bytes = zlib_inflate(&bytes)?;
    }
    // Re-check after the S/O window and inflate: a zlib bomb (or any payload the
    // wire could never carry) stops here.
    if bytes.len() as u64 > MAX_IMAGE_BYTES {
        return None;
    }
    Some(bytes)
}

/// Turn transmitted bytes into the segment that places them. `px` is required
/// for a raw framebuffer (it has no header); `lineage` groups the decode with
/// its predecessors (see [`DeferredImage::lineage`]).
fn kitty_segment(
    bytes: Vec<u8>,
    fmt: KittyFmt,
    px: Option<(u32, u32)>,
    lineage: u64,
    place: Placement,
) -> Option<Segment> {
    match fmt {
        // PNG passes through undecoded — cheap, stays inline.
        KittyFmt::Png => Some(Segment::Image(Image {
            mime: sniff_mime(&bytes)?.to_string(),
            px: dims(&bytes),
            bytes,
            place,
        })),
        // Raw framebuffers need a PNG encode — deferred to the worker (dims come
        // from the transmission params, so the placement can be stamped immediately).
        KittyFmt::Rgba | KittyFmt::Rgb => {
            let (w, h) = px?;
            let channels = if matches!(fmt, KittyFmt::Rgba) { 4 } else { 3 };
            Some(Segment::Deferred(DeferredImage {
                payload: DeferredPayload::Raw {
                    pixels: bytes,
                    channels,
                    w,
                    h,
                },
                px: (w, h),
                lineage,
                place,
            }))
        }
        KittyFmt::Unsupported => None,
    }
}

/// Build the kitty error response for a refused file/shm transmission, echoing the
/// request's image id (`i`) and/or number (`I`) so the client correlates it. Any
/// error code makes a detect-mode client treat the medium as unsupported and fall
/// back to direct transmission; `EBADF` (the code kitty uses for an unreadable
/// transmission) is the honest one. File/shm queries and real transmissions are
/// answered the same — either way we decline that unsafe medium.
fn kitty_reject(ctrl: &std::collections::HashMap<String, String>) -> Vec<u8> {
    kitty_reply(ctrl, "EBADF:file transmission not supported")
}

/// Build a kitty response, echoing the identifiers used to correlate queries.
fn kitty_reply(ctrl: &std::collections::HashMap<String, String>, status: &str) -> Vec<u8> {
    let mut keys = String::new();
    if let Some(i) = ctrl.get("i") {
        keys.push_str(&format!("i={i}"));
    }
    if let Some(n) = ctrl.get("I") {
        if !keys.is_empty() {
            keys.push(',');
        }
        keys.push_str(&format!("I={n}"));
    }
    format!("\x1b_G{keys};{status}\x1b\\").into_bytes()
}

/// Inflate a zlib stream (kitty `o=z` payloads).
fn zlib_inflate(data: &[u8]) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut out = Vec::new();
    flate2::read::ZlibDecoder::new(data)
        .read_to_end(&mut out)
        .ok()?;
    Some(out)
}

/// Encode a raw pixel buffer as PNG (via the `png` crate). `channels` is 3 (RGB) or
/// 4 (RGBA); the buffer must hold at least `width*height*channels` bytes.
fn encode_png(width: u32, height: u32, channels: u8, pixels: &[u8], fast: bool) -> Option<Vec<u8>> {
    let color = match channels {
        3 => png::ColorType::Rgb,
        4 => png::ColorType::Rgba,
        _ => return None,
    };
    let need = (width as usize)
        .checked_mul(height as usize)?
        .checked_mul(channels as usize)?;
    if width == 0 || height == 0 || pixels.len() < need {
        return None;
    }
    let mut out = Vec::new();
    let mut enc = png::Encoder::new(&mut out, width, height);
    enc.set_color(color);
    enc.set_depth(png::BitDepth::Eight);
    // Backpressure trades bytes for speed: Fast deflate keeps video-rate
    // frames flowing; the default level is for stills, where size wins.
    if fast {
        enc.set_compression(png::Compression::Fast);
    }
    let mut writer = enc.write_header().ok()?;
    // Slice to the exact expected length — a sender may pad the buffer.
    writer.write_image_data(&pixels[..need]).ok()?;
    writer.finish().ok()?;
    Some(out)
}

#[derive(Clone, Copy)]
enum Marker {
    Iterm,
    Kitty,
    Sixel,
}

/// Classify a DCS beginning at `s` (`s` starts with `ESC P`): `Some(true)` = sixel
/// (`ESC P <numeric params> q …`), `Some(false)` = some other DCS to pass through,
/// `None` = the header runs past this read and must be carried and retried.
fn sixel_dcs(s: &[u8]) -> Option<bool> {
    let mut j = DCS.len();
    while j < s.len() && (s[j].is_ascii_digit() || s[j] == b';') {
        j += 1;
    }
    match s.get(j) {
        Some(b'q') => Some(true),
        Some(_) => Some(false),
        None => None, // params reach the buffer end — need more bytes
    }
}

/// Package a sixel DCS (`ESC P … q … ST`). The full decode + PNG encode is
/// expensive (this is the video path), so when the sequence declares its
/// dimensions in raster attributes — every mainstream emitter does — it is
/// DEFERRED: the placement stamps now, a worker decodes later. A sequence
/// without raster attributes falls back to the old inline decode (rare, and
/// only that emitter pays). Sixel carries no cell size either way; `pty.rs`
/// derives one from the pixel dimensions.
/// Cell footprint of a `w`×`h` px image at cell size `cell`, so the transcode
/// occupies the same cells a sixel would (and advances the cursor accordingly).
/// Clamped to the diacritics table so every cell is addressable by a placeholder.
fn cells_for(w: u32, h: u32, cell: (u16, u16)) -> (u16, u16) {
    let (cw, ch) = (u32::from(cell.0.max(1)), u32::from(cell.1.max(1)));
    let max = DIACRITICS.len() as u32;
    let clamp = |n: u32| n.clamp(1, max) as u16;
    (clamp(w.div_ceil(cw)), clamp(h.div_ceil(ch)))
}

/// The kitty Unicode-placeholder cell character.
const PLACEHOLDER: char = '\u{10EEEE}';

/// kitty's rowcolumn diacritics: index → combining code point, encoding a
/// placeholder cell's row/column (vendored from kitty `gen/rowcolumn-diacritics.txt`).
#[rustfmt::skip]
const DIACRITICS: &[u32] = &[
    0x0305, 0x030D, 0x030E, 0x0310, 0x0312, 0x033D, 0x033E, 0x033F, 0x0346, 0x034A,
    0x034B, 0x034C, 0x0350, 0x0351, 0x0352, 0x0357, 0x035B, 0x0363, 0x0364, 0x0365,
    0x0366, 0x0367, 0x0368, 0x0369, 0x036A, 0x036B, 0x036C, 0x036D, 0x036E, 0x036F,
    0x0483, 0x0484, 0x0485, 0x0486, 0x0487, 0x0592, 0x0593, 0x0594, 0x0595, 0x0597,
    0x0598, 0x0599, 0x059C, 0x059D, 0x059E, 0x059F, 0x05A0, 0x05A1, 0x05A8, 0x05A9,
    0x05AB, 0x05AC, 0x05AF, 0x05C4, 0x0610, 0x0611, 0x0612, 0x0613, 0x0614, 0x0615,
    0x0616, 0x0617, 0x0657, 0x0658, 0x0659, 0x065A, 0x065B, 0x065D, 0x065E, 0x06D6,
    0x06D7, 0x06D8, 0x06D9, 0x06DA, 0x06DB, 0x06DC, 0x06DF, 0x06E0, 0x06E1, 0x06E2,
    0x06E4, 0x06E7, 0x06E8, 0x06EB, 0x06EC, 0x0730, 0x0732, 0x0733, 0x0735, 0x0736,
    0x073A, 0x073D, 0x073F, 0x0740, 0x0741, 0x0743, 0x0745, 0x0747, 0x0749, 0x074A,
    0x07EB, 0x07EC, 0x07ED, 0x07EE, 0x07EF, 0x07F0, 0x07F1, 0x07F3, 0x0816, 0x0817,
    0x0818, 0x0819, 0x081B, 0x081C, 0x081D, 0x081E, 0x081F, 0x0820, 0x0821, 0x0822,
    0x0823, 0x0825, 0x0826, 0x0827, 0x0829, 0x082A, 0x082B, 0x082C, 0x082D, 0x0951,
    0x0953, 0x0954, 0x0F82, 0x0F83, 0x0F86, 0x0F87, 0x135D, 0x135E, 0x135F, 0x17DD,
    0x193A, 0x1A17, 0x1A75, 0x1A76, 0x1A77, 0x1A78, 0x1A79, 0x1A7A, 0x1A7B, 0x1A7C,
    0x1B6B, 0x1B6D, 0x1B6E, 0x1B6F, 0x1B70, 0x1B71, 0x1B72, 0x1B73, 0x1CD0, 0x1CD1,
    0x1CD2, 0x1CDA, 0x1CDB, 0x1CE0, 0x1DC0, 0x1DC1, 0x1DC3, 0x1DC4, 0x1DC5, 0x1DC6,
    0x1DC7, 0x1DC8, 0x1DC9, 0x1DCB, 0x1DCC, 0x1DD1, 0x1DD2, 0x1DD3, 0x1DD4, 0x1DD5,
    0x1DD6, 0x1DD7, 0x1DD8, 0x1DD9, 0x1DDA, 0x1DDB, 0x1DDC, 0x1DDD, 0x1DDE, 0x1DDF,
    0x1DE0, 0x1DE1, 0x1DE2, 0x1DE3, 0x1DE4, 0x1DE5, 0x1DE6, 0x1DFE, 0x20D0, 0x20D1,
    0x20D4, 0x20D5, 0x20D6, 0x20D7, 0x20DB, 0x20DC, 0x20E1, 0x20E7, 0x20E9, 0x20F0,
    0x2CEF, 0x2CF0, 0x2CF1, 0x2DE0, 0x2DE1, 0x2DE2, 0x2DE3, 0x2DE4, 0x2DE5, 0x2DE6,
    0x2DE7, 0x2DE8, 0x2DE9, 0x2DEA, 0x2DEB, 0x2DEC, 0x2DED, 0x2DEE, 0x2DEF, 0x2DF0,
    0x2DF1, 0x2DF2, 0x2DF3, 0x2DF4, 0x2DF5, 0x2DF6, 0x2DF7, 0x2DF8, 0x2DF9, 0x2DFA,
    0x2DFB, 0x2DFC, 0x2DFD, 0x2DFE, 0x2DFF, 0xA66F, 0xA67C, 0xA67D, 0xA6F0, 0xA6F1,
    0xA8E0, 0xA8E1, 0xA8E2, 0xA8E3, 0xA8E4, 0xA8E5, 0xA8E6, 0xA8E7, 0xA8E8, 0xA8E9,
    0xA8EA, 0xA8EB, 0xA8EC, 0xA8ED, 0xA8EE, 0xA8EF, 0xA8F0, 0xA8F1, 0xAAB0, 0xAAB2,
    0xAAB3, 0xAAB7, 0xAAB8, 0xAABE, 0xAABF, 0xAAC1, 0xFE20, 0xFE21, 0xFE22, 0xFE23,
    0xFE24, 0xFE25, 0xFE26, 0x10A0F, 0x10A38, 0x1D185, 0x1D186, 0x1D187, 0x1D188,
    0x1D189, 0x1D1AA, 0x1D1AB, 0x1D1AC, 0x1D1AD, 0x1D242, 0x1D243, 0x1D244,
];

/// EXPERIMENTAL: encode a PNG as a kitty image the terminal renders on UNICODE
/// PLACEHOLDER cells, so it lives in the grid — scrolls with text, is cleared when
/// the cells are overwritten (no ghosts), and survives tmux. On first sight `png`
/// is transmitted as a virtual placement `i=id` (chunked); re-emissions pass `None`
/// and just re-lay the placeholder cells for the same id. The cells carry the id in
/// a 24-bit fg colour and each cell's row/col via diacritics; row transitions use
/// relative moves so the block lands wherever the cursor is.
fn kitty_placeholder(id: u32, (cols, rows): (u16, u16), png: Option<&[u8]>) -> Vec<u8> {
    let mut out = Vec::new();
    if let Some(png) = png {
        let b64 = B64.encode(png);
        let chunks: Vec<&[u8]> = b64.as_bytes().chunks(4096).collect();
        let n = chunks.len().max(1);
        for (i, ch) in chunks.iter().enumerate() {
            let more = u8::from(i + 1 < n);
            out.extend_from_slice(KITTY);
            if i == 0 {
                out.extend_from_slice(
                    format!("a=T,U=1,i={id},f=100,c={cols},r={rows},q=2,m={more}").as_bytes(),
                );
            } else {
                out.extend_from_slice(format!("m={more}").as_bytes());
            }
            out.push(b';');
            out.extend_from_slice(ch);
            out.extend_from_slice(b"\x1b\\");
        }
    }
    // fg colour = the 24-bit image id, so kitty maps the placeholders to `i=id`.
    let (r, g, b) = ((id >> 16) & 0xff, (id >> 8) & 0xff, id & 0xff);
    out.extend_from_slice(format!("\x1b[38;2;{r};{g};{b}m").as_bytes());
    let mut buf = [0u8; 4];
    for row in 0..rows {
        for col in 0..cols {
            out.extend_from_slice(PLACEHOLDER.encode_utf8(&mut buf).as_bytes());
            push_char(&mut out, DIACRITICS[row as usize], &mut buf);
            push_char(&mut out, DIACRITICS[col as usize], &mut buf);
        }
        // Back to the start column and down one — position-independent layout.
        out.extend_from_slice(format!("\x1b[{cols}D\x1b[1B").as_bytes());
    }
    // Leave the cursor at the image's bottom-left (a following newline drops below).
    out.extend_from_slice(b"\x1b[1A\x1b[39m");
    out
}

/// Append a Unicode scalar (a rowcolumn diacritic) as UTF-8.
fn push_char(out: &mut Vec<u8>, cp: u32, buf: &mut [u8; 4]) {
    if let Some(c) = char::from_u32(cp) {
        out.extend_from_slice(c.encode_utf8(buf).as_bytes());
    }
}

/// Wrap a PNG in an iTerm2 inline-image OSC 1337. iTerm2 inline images already flow
/// with the text grid (scroll, clear on overwrite), so no placeholder trick needed.
fn iterm_encode(png: &[u8], (cols, rows): (u16, u16)) -> Vec<u8> {
    format!(
        "\x1b]1337;File=inline=1;width={cols};height={rows}:{}\x07",
        B64.encode(png)
    )
    .into_bytes()
}

/// A fast content hash of a sixel DCS, to recognise the same image re-emitted by
/// tmux on every redraw (reuse the kitty id + decoded PNG instead of redoing both).
fn hash_bytes(b: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    b.hash(&mut h);
    h.finish()
}

/// Decode a sixel DCS into an image segment, or `None` when malformed (the caller
/// renders it as [`Segment::Drop`] — the local terminal still drew it).
fn sixel_image(seq: &[u8]) -> Option<Segment> {
    if let Some(px) = sixel_raster_dims(seq) {
        return Some(Segment::Deferred(DeferredImage {
            payload: DeferredPayload::Sixel(seq.to_vec()),
            px,
            lineage: 0,
            place: Placement::default(),
        }));
    }
    let img = icy_sixel::SixelImage::decode(seq).ok()?;
    let (w, h) = (
        u32::try_from(img.width).ok()?,
        u32::try_from(img.height).ok()?,
    );
    let png = encode_png(w, h, 4, &img.pixels, false)?;
    Some(Segment::Image(Image {
        mime: "image/png".to_string(),
        bytes: png,
        px: Some((w, h)),
        place: Placement::default(),
    }))
}

/// Pixel dimensions from a sixel sequence's raster attributes — the `"` item
/// (`" Pan;Pad;Ph;Pv`) directly after the `q`, giving Ph×Pv without decoding
/// a single pixel. `None` when absent (or zero-sized): the caller decodes
/// inline. The declared size is trusted like a real sixel terminal trusts it
/// (it allots the scroll region from these before pixels arrive).
fn sixel_raster_dims(seq: &[u8]) -> Option<(u32, u32)> {
    let q = seq.iter().position(|&b| b == b'q')?;
    let rest = &seq[q + 1..];
    if rest.first() != Some(&b'"') {
        return None;
    }
    let mut params: [u32; 4] = [0; 4];
    let mut i = 0; // param index
    for &b in &rest[1..] {
        match b {
            b'0'..=b'9' => {
                params[i] = params[i].saturating_mul(10) + u32::from(b - b'0');
            }
            b';' => {
                i += 1;
                if i >= 4 {
                    break;
                }
            }
            _ => break, // first sixel data byte ends the raster attributes
        }
    }
    (params[2] > 0 && params[3] > 0).then_some((params[2], params[3]))
}

/// A single-shot iTerm2 `File=<k=v>;…:<base64>` payload (verb and terminator
/// already stripped): `args` is `name=…;width=…;height=…;inline=1:<base64>`.
fn iterm_single(args: &[u8]) -> Option<Image> {
    // Split params from base64 at the first ':'.
    let colon = args.iter().position(|&b| b == b':')?;
    let params = &args[..colon];
    let b64 = std::str::from_utf8(&args[colon + 1..]).ok()?.trim();
    let kv = parse_kv(params, b';');
    // `inline=0` (the default) is a *download* — iTerm2 saves the file and renders
    // nothing inline. Displaying it would show viewers a (possibly private) file
    // the local screen never showed, and desync the mirrored cursor.
    if kv.get("inline").map(String::as_str) != Some("1") {
        return None;
    }
    let cells = cells_from(kv.get("width"), kv.get("height"));
    image_from_b64(b64.to_string(), cells)
}

/// Decode base64, sniff the format, and package it — or `None` if it isn't a
/// browser-native image.
fn image_from_b64(base64: String, cells: Option<(u16, u16)>) -> Option<Image> {
    if base64.trim().len() > MAX_B64_BYTES {
        return None; // could never fit the wire — don't even decode it
    }
    let bytes = B64.decode(base64.trim()).ok()?;
    let mime = sniff_mime(&bytes)?;
    Some(Image {
        mime: mime.to_string(),
        px: dims(&bytes),
        bytes,
        place: Placement {
            cells,
            ..Placement::default()
        },
    })
}

fn sniff_mime(b: &[u8]) -> Option<&'static str> {
    if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if b.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if b.starts_with(b"RIFF") && b.get(8..12) == Some(b"WEBP") {
        Some("image/webp")
    } else {
        None
    }
}

/// Parse `k=v<sep>k=v…` into a map. Values without `=` are ignored.
fn parse_kv(s: &[u8], sep: u8) -> std::collections::HashMap<String, String> {
    let s = std::str::from_utf8(s).unwrap_or("");
    s.split(sep as char)
        .filter_map(|part| part.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect()
}

/// A cols/rows display hint, only when both are plain *nonzero* integers (cells) —
/// iTerm2's `Npx`/`N%`/`auto`, kitty pixel sizing, and a zero fall back to natural
/// size. Zero matters: both protocols define `0` as "unspecified", not a
/// zero-extent image.
fn cells_from(c: Option<&String>, r: Option<&String>) -> Option<(u16, u16)> {
    let c = c?.parse::<u16>().ok().filter(|&v| v > 0)?;
    let r = r?.parse::<u16>().ok().filter(|&v| v > 0)?;
    Some((c, r))
}

/// How the scan for an image sequence's end came out.
enum StringEnd {
    /// Terminated normally; offset one past the terminator.
    End(usize),
    /// Cancelled mid-string — CAN/SUB, or an ESC that doesn't form ST — the way
    /// vte and real terminals give up on a string that never terminates. Offset
    /// to resume scanning at: the cancelling ESC itself (it starts whatever comes
    /// next), or one past a CAN/SUB.
    Abort(usize),
    /// Runs past the buffer; scanning may resume at this offset next read (it
    /// points at a trailing ESC that could still become ST, else buffer end).
    More(usize),
}

/// Scan `s[from..]` for the end of an image sequence. iTerm2 (OSC) ends at BEL or
/// ST; kitty (APC) at ST (`ESC \`); sixel (DCS) at ST or 8-bit `0x9c`. All three
/// payloads are base64/sixel data, which never contains ESC/CAN/SUB — so any of
/// those mid-string is unambiguously a cancellation, not data. `from` must skip a
/// leading marker ESC (pass 1 when `s` starts at the marker).
fn scan_string(s: &[u8], marker: Marker, from: usize) -> StringEnd {
    let mut i = from;
    while i < s.len() {
        match s[i] {
            0x07 if matches!(marker, Marker::Iterm) => return StringEnd::End(i + 1), // BEL
            0x9c if matches!(marker, Marker::Sixel) => return StringEnd::End(i + 1), // 8-bit ST
            0x18 | 0x1a => return StringEnd::Abort(i + 1), // CAN/SUB cancel the string
            0x1b if i + 1 < s.len() && s[i + 1] == b'\\' => return StringEnd::End(i + 2), // ST
            0x1b if i + 1 == s.len() => return StringEnd::More(i), // ESC at edge: ST or cancel?
            0x1b => return StringEnd::Abort(i),            // any other ESC cancels the string
            _ => i += 1,
        }
    }
    StringEnd::More(s.len())
}

fn strip_terminator(body: &[u8]) -> &[u8] {
    if body.ends_with(b"\x1b\\") {
        &body[..body.len() - 2]
    } else if body.ends_with(b"\x07") {
        &body[..body.len() - 1]
    } else {
        body
    }
}

/// Emit a passthrough [`Step`]: non-image bytes that tee to the local terminal
/// AND feed vt100.
fn push_pass(out: &mut Vec<Step>, bytes: &[u8]) {
    if !bytes.is_empty() {
        out.push(Step::Passthrough(bytes.to_vec()));
    }
}

/// Route a parsed sequence into a [`Step`]: its original bytes tee to the local
/// terminal (which renders it natively) — EXCEPT a [`Segment::Reject`], which is
/// suppressed (no tee) so the terminal never services the refused file/shm transfer.
fn push_tee(out: &mut Vec<Step>, bytes: &[u8], seg: Segment) {
    out.push(match seg {
        Segment::Image(i) => Step::Image(bytes.to_vec(), i),
        Segment::Deferred(d) => Step::Deferred(bytes.to_vec(), d),
        Segment::Query(r) => Step::Query(bytes.to_vec(), r),
        Segment::Delete(sel) => Step::Delete(bytes.to_vec(), sel),
        Segment::Drop => Step::TerminalOnly(bytes.to_vec()),
        Segment::Reject(r) => Step::Reject(r),
        // Transcode: the tee is the re-encoded bytes, NOT the original sixel.
        Segment::Transcoded { tee, image } => Step::Image(tee, image),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    // 1x1 transparent PNG.
    fn png_b64() -> String {
        B64.encode([
            0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1f, 0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0a, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9c, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00,
            0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
        ])
    }

    fn only_images(steps: Vec<Step>) -> Vec<Image> {
        steps
            .into_iter()
            .filter_map(|s| match s {
                Step::Image(_, i) => Some(i),
                // Materialize deferred payloads exactly like the pty worker
                // does, so the decode round-trip assertions keep covering
                // the sixel / kitty-raw paths.
                Step::Deferred(_, d) => {
                    let (bytes, px) = finish_deferred(&d.payload, false)?;
                    Some(Image {
                        mime: "image/png".to_string(),
                        bytes,
                        px: Some(px),
                        place: d.place,
                    })
                }
                Step::Passthrough(_)
                | Step::TerminalOnly(_)
                | Step::Query(_, _)
                | Step::Delete(_, _)
                | Step::Reject(_) => None,
            })
            .collect()
    }

    /// Concatenate the local-terminal tee bytes across steps — what the operator's
    /// terminal sees (passthrough + rendered sequences, minus rejected ones).
    fn tee_bytes(steps: &[Step]) -> Vec<u8> {
        steps
            .iter()
            .filter_map(Step::tee)
            .flatten()
            .copied()
            .collect()
    }

    /// The injected app-bound responses (kitty rejections) across steps.
    fn rejections(steps: &[Step]) -> Vec<Vec<u8>> {
        steps
            .iter()
            .filter_map(|s| match s {
                Step::Reject(r) => Some(r.clone()),
                _ => None,
            })
            .collect()
    }

    // The heavy paths must come out DEFERRED (stamp now, decode on the
    // worker): sixel with raster attributes carries its dims up front; a
    // kitty raw framebuffer carries them in the transmission params. The
    // fast-compression variant must still be a valid PNG.
    #[test]
    fn heavy_paths_defer_with_upfront_dims() {
        let mut it = Interceptor::new();
        let sixel = b"\x1bPq\"1;1;4;2#0;2;100;0;0#0~~~~$-\x1b\\";
        let segs = it.feed(sixel);
        let d = segs
            .iter()
            .find_map(|s| match s {
                Step::Deferred(_, d) => Some(d.clone()),
                _ => None,
            })
            .expect("sixel with raster attributes defers");
        assert_eq!(d.px, (4, 2), "dims from raster attributes, no decode");
        assert!(matches!(d.payload, DeferredPayload::Sixel(_)));
        // Both compression levels produce decodable PNGs of the same size.
        let (norm, px) = finish_deferred(&d.payload, false).expect("decodes");
        let (fast, px2) = finish_deferred(&d.payload, true).expect("decodes fast");
        assert_eq!(px, px2);
        for png_bytes in [&norm, &fast] {
            let dec = png::Decoder::new(std::io::Cursor::new(png_bytes));
            let reader = dec.read_info().expect("valid PNG");
            assert_eq!((reader.info().width, reader.info().height), (px.0, px.1));
        }

        let mut it = Interceptor::new();
        let raw = [0u8; 2 * 2 * 4];
        let s = format!("\x1b_Ga=T,f=32,t=d,s=2,v=2;{}\x1b\\", B64.encode(raw));
        let kitty = it.feed(s.as_bytes());
        assert!(
            kitty.iter().any(|s| matches!(
                s,
                Step::Deferred(
                    _,
                    DeferredImage {
                        payload: DeferredPayload::Raw { .. },
                        px: (2, 2),
                        ..
                    }
                )
            )),
            "kitty raw framebuffer defers with metadata dims"
        );
    }

    #[test]
    fn iterm_inline_image_extracted_with_passthrough() {
        let mut it = Interceptor::new();
        let b64 = png_b64();
        let stream = format!("hi\x1b]1337;File=inline=1;width=4;height=2:{b64}\x07bye");
        let segs = it.feed(stream.as_bytes());

        // Text before and after passes through; image pulled out with size hint.
        assert_eq!(only_pass(&segs), b"hibye");
        let imgs = only_images(segs);
        assert_eq!(imgs.len(), 1);
        assert_eq!(imgs[0].mime, "image/png");
        assert_eq!(imgs[0].place.cells, Some((4, 2)));
        assert_eq!(imgs[0].bytes, B64.decode(&b64).unwrap());
    }

    #[test]
    fn iterm_multipart_image_reassembled() {
        // MultipartFile (metadata, no data) → FilePart* (base64 chunks) → FileEnd.
        // Only FileEnd yields the image; the cell hint comes from the opener.
        let mut it = Interceptor::new();
        let b64 = png_b64();
        let (p1, p2) = b64.split_at(9);
        let mut segs = it.feed(b"\x1b]1337;MultipartFile=inline=1;width=4;height=2\x07");
        segs.extend(it.feed(format!("\x1b]1337;FilePart={p1}\x07").as_bytes()));
        segs.extend(it.feed(format!("\x1b]1337;FilePart={p2}\x07").as_bytes()));
        assert!(only_images(segs).is_empty(), "no image until FileEnd");
        let end = it.feed(b"\x1b]1337;FileEnd\x07");
        let imgs = only_images(end);
        assert_eq!(imgs.len(), 1);
        assert_eq!(imgs[0].mime, "image/png");
        assert_eq!(imgs[0].place.cells, Some((4, 2)));
        assert_eq!(imgs[0].bytes, B64.decode(&b64).unwrap());
    }

    #[test]
    fn gated_off_protocol_passes_through() {
        // Kitty + sixel off (terminal doesn't support them): those sequences are not
        // consumed — they pass through to vt100, matching what the terminal shows.
        let mut it = Interceptor::with(false, true, false, None, (10, 20));
        let s = format!("\x1b_Ga=T,f=100,t=d;{}\x1b\\", png_b64());
        let sixel = icy_sixel::SixelImage::from_rgba(vec![255, 0, 0, 255], 1, 1)
            .encode()
            .unwrap();
        let stream = format!("{s}{sixel}");
        let segs = it.feed(stream.as_bytes());
        assert!(only_images(segs.clone()).is_empty());
        let passed = only_pass(&segs);
        assert_eq!(
            passed,
            stream.as_bytes(),
            "gated-off bytes reach vt100 verbatim"
        );
        // iTerm2 is still intercepted (its flag is on).
        let mut it2 = Interceptor::with(false, true, false, None, (10, 20));
        let s2 = format!("\x1b]1337;File=inline=1:{}\x07", png_b64());
        assert_eq!(only_images(it2.feed(s2.as_bytes())).len(), 1);
    }

    #[test]
    fn sixel_dcs_decoded_to_png() {
        // Round-trip: encode a 2x2 image to a sixel DCS, feed it through, and confirm
        // it comes back out as a PNG with pixel dims (no cell hint → derived later).
        let rgba = vec![
            255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 0, 255,
        ];
        let sixel = icy_sixel::SixelImage::from_rgba(rgba, 2, 2)
            .encode()
            .unwrap();
        assert!(sixel.as_bytes().starts_with(DCS), "emitter produces a DCS");
        let mut it = Interceptor::new();
        let imgs = only_images(it.feed(sixel.as_bytes()));
        assert_eq!(imgs.len(), 1);
        assert_eq!(imgs[0].mime, "image/png");
        // Sixel packs rows into 6px bands, so a round-tripped 2x2 can come back
        // padded to 2x6 — we don't assert exact dims, just that they're carried.
        let (w, h) = imgs[0].px.unwrap();
        assert_eq!(w, 2);
        assert!(h >= 2);
        assert_eq!(imgs[0].place.cells, None);
    }

    fn sample_sixel() -> Vec<u8> {
        let rgba = vec![
            255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 0, 255,
        ];
        icy_sixel::SixelImage::from_rgba(rgba, 2, 2)
            .encode()
            .unwrap()
            .into_bytes()
    }

    #[test]
    fn sixel_transcodes_to_kitty_placeholders_when_terminal_lacks_sixel() {
        // EXPERIMENTAL: kitty-graphics terminal, no native sixel → the sixel is
        // decoded and re-teed as a kitty VIRTUAL PLACEMENT + Unicode placeholder
        // cells (so it scrolls with the grid), while the mirror still gets a PNG.
        let sixel = sample_sixel();
        let mut it = Interceptor::with(true, false, true, Some(GfxProto::Kitty), (10, 20));
        let steps = it.feed(&sixel);
        let tee = tee_bytes(&steps);
        assert!(contains(&tee, b"\x1b_G"), "transmits a kitty image");
        assert!(
            contains(&tee, b"U=1"),
            "as a virtual placement (Unicode placeholder)"
        );
        assert!(
            contains(&tee, "\u{10EEEE}".as_bytes()),
            "lays down placeholder cells"
        );
        assert!(
            !contains(&tee, DCS),
            "no raw sixel DCS reaches the terminal"
        );
        assert_eq!(only_images(steps).len(), 1, "mirror still gets one image");
    }

    #[test]
    fn kitty_transcode_reuses_id_on_reemission() {
        // tmux re-emits the same sixel on every scroll: the second time carries the
        // placeholder cells but NOT another image transmission (id + PNG are reused).
        let sixel = sample_sixel();
        let mut it = Interceptor::with(true, false, true, Some(GfxProto::Kitty), (10, 20));
        let first = tee_bytes(&it.feed(&sixel));
        assert!(contains(&first, b"a=T,U=1"), "first sight transmits");
        let again = tee_bytes(&it.feed(&sixel));
        assert!(
            !contains(&again, b"\x1b_G"),
            "re-emission does not re-transmit"
        );
        assert!(
            contains(&again, "\u{10EEEE}".as_bytes()),
            "re-emission still lays placeholders"
        );
    }

    #[test]
    fn sixel_transcodes_to_iterm_when_terminal_lacks_sixel() {
        let sixel = sample_sixel();
        let mut it = Interceptor::with(false, true, true, Some(GfxProto::Iterm), (10, 20));
        let tee = tee_bytes(&it.feed(&sixel));
        assert!(
            contains(&tee, b"\x1b]1337;File=inline=1"),
            "tee is an iTerm2 OSC"
        );
        assert!(!contains(&tee, DCS));
    }

    #[test]
    fn native_sixel_terminal_keeps_the_raw_fast_path() {
        // transcode=None: the deferred path stays, tee is the sixel verbatim.
        let sixel = sample_sixel();
        let mut it = Interceptor::with(false, false, true, None, (10, 20));
        assert_eq!(tee_bytes(&it.feed(&sixel)), sixel, "sixel teed unchanged");
    }

    fn contains(hay: &[u8], needle: &[u8]) -> bool {
        hay.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn sixel_dcs_classification() {
        assert_eq!(sixel_dcs(b"\x1bP0;1;0q\"1;1"), Some(true)); // sixel
        assert_eq!(sixel_dcs(b"\x1bP$qm"), Some(false)); // DECRQSS, not sixel
        assert_eq!(sixel_dcs(b"\x1bP0;1;0"), None); // header split before the `q`
        assert_eq!(sixel_dcs(DCS), None);
    }

    #[test]
    fn iterm_orphan_filepart_ignored() {
        // A FilePart/FileEnd with no opening MultipartFile must not panic or emit.
        let mut it = Interceptor::new();
        let mut segs = it.feed(b"\x1b]1337;FilePart=AAAA\x07");
        segs.extend(it.feed(b"\x1b]1337;FileEnd\x07"));
        assert!(only_images(segs).is_empty());
    }

    #[test]
    fn image_carries_pixel_dims() {
        // px lets pty.rs derive a cell count (and advance the cursor) for an image
        // with no app-given size. png_b64 is a 1x1 PNG.
        let mut it = Interceptor::new();
        let s = format!("\x1b]1337;File=inline=1:{}\x07", png_b64());
        let imgs = only_images(it.feed(s.as_bytes()));
        assert_eq!(imgs[0].px, Some((1, 1)));
    }

    #[test]
    fn sequence_split_across_two_reads() {
        let mut it = Interceptor::new();
        let b64 = png_b64();
        let full = format!("\x1b]1337;File=inline=1:{b64}\x07");
        let (a, b) = full.as_bytes().split_at(10); // mid-sequence boundary
        let mut segs = it.feed(a);
        segs.extend(it.feed(b));
        assert_eq!(only_images(segs).len(), 1);
    }

    #[test]
    fn kitty_direct_png_chunked() {
        let mut it = Interceptor::new();
        let b64 = png_b64();
        let (p1, p2) = b64.split_at(8);
        // First chunk carries control (f=100 PNG, direct, c/r cells) + m=1; second m=0.
        let s1 = format!("\x1b_Ga=T,f=100,t=d,c=3,r=1,m=1;{p1}\x1b\\");
        let s2 = format!("\x1b_Gm=0;{p2}\x1b\\");
        let mut segs = it.feed(s1.as_bytes());
        segs.extend(it.feed(s2.as_bytes()));
        let imgs = only_images(segs);
        assert_eq!(imgs.len(), 1);
        assert_eq!(imgs[0].mime, "image/png");
        assert_eq!(imgs[0].place.cells, Some((3, 1)));
    }

    #[test]
    fn kitty_raw_rgba_reencoded_as_png() {
        let mut it = Interceptor::new();
        // 2x2 raw RGBA with s/v pixel dims — must come out as a PNG that decodes
        // back to the exact same pixels (round-trip through the real png crate).
        let src = [
            255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
        ];
        let s = format!(
            "\x1b_Ga=T,f=32,t=d,s=2,v=2,c=1,r=1;{}\x1b\\",
            B64.encode(src)
        );
        let imgs = only_images(it.feed(s.as_bytes()));
        assert_eq!(imgs.len(), 1);
        assert_eq!(imgs[0].mime, "image/png");

        let png = imgs[0].bytes.clone();
        let decoder = png::Decoder::new(std::io::Cursor::new(&png));
        let mut reader = decoder.read_info().expect("valid PNG");
        assert_eq!((reader.info().width, reader.info().height), (2, 2));
        assert_eq!(reader.info().color_type, png::ColorType::Rgba);
        let mut buf = vec![0; reader.output_buffer_size().unwrap()];
        let frame = reader.next_frame(&mut buf).unwrap();
        assert_eq!(
            &buf[..frame.buffer_size()],
            &src[..],
            "pixels round-trip exactly"
        );
    }

    #[test]
    fn kitty_unknown_and_indirect_mediums_dropped() {
        let mut it = Interceptor::new();
        // File transport is refused wholesale (never read) — see
        // file_and_shm_mediums_never_read_the_filesystem for the security intent.
        let noent = B64.encode(b"/nonexistent/sg/does-not-exist.png");
        let s = format!("\x1b_Ga=T,f=100,t=f;{noent}\x1b\\");
        assert!(only_images(it.feed(s.as_bytes())).is_empty());
        // An unknown transmission medium is dropped.
        let s = format!("\x1b_Ga=T,f=100,t=x;{}\x1b\\", png_b64());
        assert!(only_images(it.feed(s.as_bytes())).is_empty());
    }

    /// The bytes vt100 sees — [`Step::Passthrough`] only, NOT the original bytes of
    /// rendered sequences (those tee to the terminal but never reach the parser).
    fn only_pass(steps: &[Step]) -> Vec<u8> {
        steps
            .iter()
            .filter_map(|s| match s {
                Step::Passthrough(b) => Some(b.clone()),
                _ => None,
            })
            .flatten()
            .collect()
    }

    #[test]
    fn unterminated_sequence_cancelled_by_next_escape() {
        // Ctrl-C mid-transfer: the app dies before sending ST. The next escape
        // sequence (the prompt repaint) cancels the string — exactly what the
        // real terminal and vt100's vte do — so the mirror keeps flowing instead
        // of buffering the rest of the session forever.
        let mut it = Interceptor::new();
        assert!(it.feed(b"\x1b_Gf=100,t=d;AAAA").is_empty());
        let segs = it.feed(b"\x1b[2K$ ls");
        assert!(only_images(segs.clone()).is_empty());
        assert_eq!(
            only_pass(&segs),
            b"\x1b[2K$ ls",
            "the cancelling bytes reach vt100"
        );
        // The interceptor is fully recovered: a complete image still comes out.
        let s = format!("\x1b]1337;File=inline=1:{}\x07", png_b64());
        assert_eq!(only_images(it.feed(s.as_bytes())).len(), 1);
    }

    #[test]
    fn can_cancels_sequence_within_one_read() {
        let mut it = Interceptor::new();
        let segs = it.feed(b"before\x1b_Gf=100,t=d;AAAA\x18after");
        assert!(only_images(segs.clone()).is_empty());
        assert_eq!(only_pass(&segs), b"beforeafter");
    }

    #[test]
    fn cancellation_clears_multipart_accumulator() {
        let mut it = Interceptor::new();
        let mut segs = it.feed(b"\x1b]1337;MultipartFile=inline=1;width=4;height=2\x07");
        // A FilePart cancelled mid-payload breaks the whole transfer…
        segs.extend(it.feed(b"\x1b]1337;FilePart=AAAA\x18"));
        // …so FileEnd must not emit a half-baked image.
        segs.extend(it.feed(b"\x1b]1337;FileEnd\x07"));
        assert!(only_images(segs).is_empty());
    }

    #[test]
    fn runaway_sequence_memory_bounded_and_dropped() {
        // A sequence that never terminates is condemned once it outgrows the cap:
        // its bytes are discarded as they stream (memory stays ~one PTY read) and
        // its eventual terminator yields nothing.
        let mut it = Interceptor::new();
        assert!(it.feed(b"\x1b_Gf=100,t=d;").is_empty());
        let chunk = [b'A'; 4096];
        for _ in 0..(MAX_SEQ_BYTES / chunk.len() + 4) {
            assert!(it.feed(&chunk).is_empty());
        }
        assert!(
            it.carried_len() <= chunk.len() + 1,
            "drain mode keeps only a tail, not {} bytes",
            it.carried_len()
        );
        // The terminator finally arrives: the condemned sequence is dropped and
        // the stream flows again.
        let segs = it.feed(b"\x1b\\after");
        assert!(only_images(segs.clone()).is_empty());
        assert_eq!(only_pass(&segs), b"after");
        let s = format!("\x1b]1337;File=inline=1:{}\x07", png_b64());
        assert_eq!(only_images(it.feed(s.as_bytes())).len(), 1);
    }

    #[test]
    fn st_split_between_reads() {
        // The two-byte ST split exactly at the read boundary — the resumed scan
        // must recheck the trailing ESC, not skip past it.
        let mut it = Interceptor::new();
        let s = format!("\x1b_Ga=T,f=100,t=d;{}", png_b64());
        let mut segs = it.feed(s.as_bytes());
        segs.extend(it.feed(b"\x1b"));
        assert!(only_images(segs.clone()).is_empty());
        segs.extend(it.feed(b"\\"));
        assert_eq!(only_images(segs).len(), 1);
    }

    #[test]
    fn large_sequence_reassembled_across_many_small_reads() {
        // A multi-hundred-KB kitty transfer arriving in 4 KB PTY reads (the real
        // shape of an `imgcat`-sized image) comes out intact — the resumed scan
        // handles arbitrary boundaries, including inside the payload.
        let mut it = Interceptor::new();
        let (w, h) = (300u32, 300u32);
        let raw = vec![0x7fu8; (w * h * 4) as usize];
        let seq = format!("\x1b_Ga=T,f=32,t=d,s={w},v={h};{}\x1b\\", B64.encode(&raw));
        let mut imgs = Vec::new();
        for chunk in seq.as_bytes().chunks(4096) {
            imgs.extend(only_images(it.feed(chunk)));
        }
        assert_eq!(imgs.len(), 1);
        assert_eq!(imgs[0].mime, "image/png");
        assert_eq!(imgs[0].px, Some((w, h)));
    }

    #[test]
    fn kitty_non_display_actions_render_nothing() {
        // Only `a=T` (transmit-and-display) draws at the cursor. A capability
        // query — including the exact probe shellglass itself sends in pty.rs —
        // a transmit-only, a delete, or an action-less sequence (spec default is
        // `t`) must not become a phantom web image.
        for seq in [
            "\x1b_Gi=1,a=q,s=1,v=1,t=d,f=24;AAAA\x1b\\".to_string(), // pty.rs's own probe
            format!("\x1b_Ga=t,f=100,t=d;{}\x1b\\", png_b64()),      // transmit-only
            format!("\x1b_Gf=100,t=d;{}\x1b\\", png_b64()),          // no action ⇒ default t
            "\x1b_Ga=d,d=A\x1b\\".to_string(),                       // delete
        ] {
            let mut it = Interceptor::new();
            assert!(
                only_images(it.feed(seq.as_bytes())).is_empty(),
                "non-display action rendered an image: {seq:?}"
            );
        }
    }

    /// A 4×4 RGBA framebuffer whose every pixel is `(x, y, 0, 255)`.
    fn ramp_rgba() -> Vec<u8> {
        (0..4u8)
            .flat_map(|y| (0..4u8).flat_map(move |x| [x, y, 0, 255]))
            .collect()
    }

    /// zellij's shape: transmit chunked with an id (nothing drawn yet), then
    /// place it after positioning the cursor itself (`C=1`).
    #[test]
    fn kitty_transmit_then_place_mirrors_the_placement() {
        let mut it = Interceptor::new();
        let b64 = B64.encode(ramp_rgba());
        let (head, tail) = b64.split_at(8);
        let transmit = format!(
            "\x1b_Ga=t,q=2,f=32,t=d,i=7,s=4,v=4,m=1;{head}\x1b\\\x1b_Gq=2,m=0;{tail}\x1b\\"
        );
        let steps = it.feed(transmit.as_bytes());
        assert!(
            only_images(steps.clone()).is_empty(),
            "a transmit draws nothing"
        );
        assert_eq!(
            tee_bytes(&steps),
            transmit.as_bytes(),
            "terminal sees it all"
        );

        let place = b"\x1b_Ga=p,q=2,i=7,p=1,x=0,y=0,w=4,h=4,X=0,Y=0,z=0,C=1\x1b\\";
        let steps = it.feed(place);
        assert_eq!(tee_bytes(&steps), place);
        let images = only_images(steps);
        assert_eq!(images.len(), 1, "the placement is mirrored");
        assert_eq!(images[0].px, Some((4, 4)));
        assert_eq!(
            images[0].place.cells, None,
            "footprint derives from the crop px"
        );
        assert!(images[0].place.hold, "C=1 keeps the cursor put");
        assert_eq!(images[0].place.key, Some((7, 1)));
    }

    #[test]
    fn kitty_place_cuts_out_the_source_rect() {
        let mut it = Interceptor::new();
        let transmit = format!(
            "\x1b_Ga=t,f=32,t=d,i=7,s=4,v=4;{}\x1b\\",
            B64.encode(ramp_rgba())
        );
        it.feed(transmit.as_bytes());
        let steps = it.feed(b"\x1b_Ga=p,i=7,p=2,x=1,y=2,w=2,h=1,C=1\x1b\\");
        let Some(Step::Deferred(_, d)) = steps.iter().find(|s| matches!(s, Step::Deferred(..)))
        else {
            panic!("expected a deferred crop, got {steps:?}");
        };
        assert_eq!(d.px, (2, 1));
        assert_eq!(
            d.payload,
            DeferredPayload::Raw {
                // row y=2, columns x=1..3
                pixels: vec![1, 2, 0, 255, 2, 2, 0, 255],
                channels: 4,
                w: 2,
                h: 1,
            }
        );
    }

    /// Distinct placements must not collapse into one decode lineage (the pty
    /// worker supersedes within a lineage), while re-placing the same one does.
    #[test]
    fn kitty_placements_carry_distinct_lineages() {
        let mut it = Interceptor::new();
        it.feed(
            format!(
                "\x1b_Ga=t,f=32,t=d,i=7,s=4,v=4;{}\x1b\\",
                B64.encode(ramp_rgba())
            )
            .as_bytes(),
        );
        let lineage = |it: &mut Interceptor, seq: &str| match it.feed(seq.as_bytes()).pop() {
            Some(Step::Deferred(_, d)) => d.lineage,
            other => panic!("expected a deferred placement, got {other:?}"),
        };
        let a = lineage(&mut it, "\x1b_Ga=p,i=7,p=1,C=1\x1b\\");
        let b = lineage(&mut it, "\x1b_Ga=p,i=7,p=2,C=1\x1b\\");
        let a_again = lineage(&mut it, "\x1b_Ga=p,i=7,p=1,C=1\x1b\\");
        assert_ne!(a, b, "two placements are two images");
        assert_eq!(a, a_again, "re-placing the same one supersedes it");
    }

    #[test]
    fn kitty_delete_selects_placements_and_frees_the_store() {
        let mut it = Interceptor::new();
        it.feed(
            format!(
                "\x1b_Ga=t,f=32,t=d,i=7,s=4,v=4;{}\x1b\\",
                B64.encode(ramp_rgba())
            )
            .as_bytes(),
        );
        let sel = |it: &mut Interceptor, seq: &str| match it.feed(seq.as_bytes()).pop() {
            Some(Step::Delete(_, sel)) => Some(sel),
            _ => None,
        };
        // Each selector kitty defines, in the shape `pty::revokes` matches on.
        // (Cell coordinates are 1-based on the wire and 0-based here.)
        assert_eq!(
            sel(&mut it, "\x1b_Ga=d,d=i,i=7,p=3\x1b\\"),
            Some(DeleteSel::Ids {
                first: 7,
                last: 7,
                placement: Some(3)
            })
        );
        assert_eq!(
            sel(&mut it, "\x1b_Ga=d,d=i,i=7\x1b\\"),
            Some(DeleteSel::Ids {
                first: 7,
                last: 7,
                placement: None
            })
        );
        assert_eq!(
            sel(&mut it, "\x1b_Ga=d,d=r,x=3,y=9\x1b\\"),
            Some(DeleteSel::Ids {
                first: 3,
                last: 9,
                placement: None
            })
        );
        assert_eq!(sel(&mut it, "\x1b_Ga=d,d=a\x1b\\"), Some(DeleteSel::All));
        assert_eq!(sel(&mut it, "\x1b_Ga=d\x1b\\"), Some(DeleteSel::All));
        assert_eq!(sel(&mut it, "\x1b_Ga=d,d=c\x1b\\"), Some(DeleteSel::Cursor));
        assert_eq!(
            sel(&mut it, "\x1b_Ga=d,d=q,x=4,y=2,z=-1\x1b\\"),
            Some(DeleteSel::At {
                col: Some(3),
                row: Some(1),
                z: Some(-1)
            })
        );
        assert_eq!(
            sel(&mut it, "\x1b_Ga=d,d=x,x=4\x1b\\"),
            Some(DeleteSel::At {
                col: Some(3),
                row: None,
                z: None
            })
        );
        assert_eq!(
            sel(&mut it, "\x1b_Ga=d,d=z,z=1\x1b\\"),
            Some(DeleteSel::At {
                col: None,
                row: None,
                z: Some(1)
            })
        );
        // Animation frames revoke no placement, so they are not a delete here.
        assert_eq!(sel(&mut it, "\x1b_Ga=d,d=f,i=7\x1b\\"), None);

        // The lowercase forms keep the data placeable; `d=I` frees it.
        assert_eq!(
            only_images(it.feed(b"\x1b_Ga=p,i=7,p=9,C=1\x1b\\")).len(),
            1
        );
        it.feed(b"\x1b_Ga=d,d=I,i=7\x1b\\");
        assert!(only_images(it.feed(b"\x1b_Ga=p,i=7,p=9,C=1\x1b\\")).is_empty());
    }

    #[test]
    fn kitty_place_of_an_unknown_id_shows_nothing() {
        let mut it = Interceptor::new();
        let place = b"\x1b_Ga=p,i=99,p=1,C=1\x1b\\";
        let steps = it.feed(place);
        assert!(only_images(steps.clone()).is_empty());
        assert_eq!(tee_bytes(&steps), place, "the terminal still draws it");
    }

    /// An encoded image can be placed whole, but a partial crop would need a
    /// decoder — and mirroring it uncropped would paint over what the terminal
    /// clipped away. So it isn't mirrored at all.
    #[test]
    fn kitty_place_of_an_encoded_image_crops_through_a_decode() {
        // A 4x4 RGBA ramp, transmitted ENCODED (f=100) rather than raw.
        let png = encode_png(4, 4, 4, &ramp_rgba(), true).expect("encode");
        let mut it = Interceptor::new();
        it.feed(format!("\x1b_Ga=t,f=100,t=d,i=8;{}\x1b\\", B64.encode(&png)).as_bytes());

        // The whole rect passes through undecoded — the stored bytes verbatim.
        let whole = only_images(it.feed(b"\x1b_Ga=p,i=8,p=1,C=1\x1b\\"));
        assert_eq!(whole.len(), 1);
        assert_eq!(whole[0].bytes, png, "no re-encode for the whole image");

        // A partial rect is decoded, cut and re-encoded — same pixels the raw
        // path produces for the same crop.
        let cut = only_images(it.feed(b"\x1b_Ga=p,i=8,p=2,x=1,y=2,w=2,h=1,C=1\x1b\\"));
        assert_eq!(cut.len(), 1);
        assert_eq!(cut[0].px, Some((2, 1)));
        let (pixels, channels, w, h) = decode_png(&cut[0].bytes).expect("decode the crop");
        assert_eq!((channels, w, h), (4, 2, 1));
        assert_eq!(pixels, vec![1, 2, 0, 255, 2, 2, 0, 255]);

        // A degenerate rect places nothing at all.
        assert!(
            only_images(it.feed(b"\x1b_Ga=p,i=8,p=3,x=0,y=0,w=1,h=0,C=1\x1b\\")).is_empty(),
            "an empty rect is not mirrored"
        );
    }

    /// The refactor that added the store must not have opened the file/shm hole:
    /// a transmit-only over an indirect medium is still refused.
    #[test]
    fn kitty_transmit_over_file_medium_is_still_rejected() {
        let mut it = Interceptor::new();
        let steps = it.feed(b"\x1b_Ga=t,f=100,t=f,i=5;L3RtcC94\x1b\\");
        assert!(matches!(steps.as_slice(), [Step::Reject(_)]));
        assert!(tee_bytes(&steps).is_empty(), "suppressed from the terminal");
        assert!(
            only_images(it.feed(b"\x1b_Ga=p,i=5,p=1,C=1\x1b\\")).is_empty(),
            "nothing was stored to place"
        );
    }

    #[test]
    fn kitty_direct_query_is_routed_with_a_correlated_cached_reply() {
        let query = b"\x1b_Gi=7,I=42,a=q,s=1,v=1,t=d,f=24;AAAA\x1b\\";
        let mut it = Interceptor::new();
        let steps = it.feed(query);
        assert_eq!(tee_bytes(&steps), query);
        assert!(matches!(
            steps.as_slice(),
            [Step::Query(_, response)]
                if response == b"\x1b_Gi=7,I=42;OK\x1b\\"
        ));
    }

    #[test]
    fn kitty_query_never_reads_referenced_file() {
        // `a=q` with a file medium must not touch the filesystem: the query asks
        // "can you?", it doesn't transmit. Point it at a real file and verify the
        // flush bails on the action before resolving the medium.
        let path = std::env::temp_dir().join("sg_query_no_read_test.png");
        std::fs::write(&path, B64.decode(png_b64()).unwrap()).unwrap();
        let b64path = B64.encode(path.to_str().unwrap().as_bytes());
        let mut it = Interceptor::new();
        let s = format!("\x1b_Ga=q,f=100,t=f;{b64path}\x1b\\");
        let imgs = only_images(it.feed(s.as_bytes()));
        let _ = std::fs::remove_file(&path);
        assert!(imgs.is_empty());
    }

    #[test]
    fn iterm_download_not_displayed() {
        // `inline=0` — and iTerm2's default of no `inline` key at all — is a file
        // *download*: the real terminal saves it and renders nothing.
        let b64 = png_b64();
        for seq in [
            format!("\x1b]1337;File=name=cGljLnBuZw==;inline=0:{b64}\x07"),
            format!("\x1b]1337;File=name=cGljLnBuZw==:{b64}\x07"),
        ] {
            let mut it = Interceptor::new();
            assert!(
                only_images(it.feed(seq.as_bytes())).is_empty(),
                "download displayed as inline image: {seq:?}"
            );
        }
    }

    #[test]
    fn iterm_multipart_download_not_displayed() {
        // A multipart transfer without inline=1 opens no accumulator, so its
        // parts and end are no-ops.
        let mut it = Interceptor::new();
        let b64 = png_b64();
        let mut segs = it.feed(b"\x1b]1337;MultipartFile=inline=0;width=4;height=2\x07");
        segs.extend(it.feed(format!("\x1b]1337;FilePart={b64}\x07").as_bytes()));
        segs.extend(it.feed(b"\x1b]1337;FileEnd\x07"));
        assert!(only_images(segs).is_empty());
    }

    #[test]
    fn file_and_shm_mediums_never_read_the_filesystem() {
        // SECURITY: an injected kitty file/shm transmission must NOT read the
        // referenced path — that was an arbitrary-file exfiltration vector (raw
        // framebuffer needs no image header, so any file's bytes become pixels).
        // Point one at a real, readable file with recognizable content and assert
        // nothing is extracted: the mirror never opens a path from the stream.
        let path = std::env::temp_dir().join("sg_exfil_probe_test");
        std::fs::write(&path, vec![0x41u8; 2 * 2 * 3]).unwrap(); // 2x2 RGB of 'A's
        let p64 = B64.encode(path.to_str().unwrap());
        for medium in ["f", "t", "s"] {
            let mut it = Interceptor::new();
            let seq = format!("\x1b_Ga=T,f=24,t={medium},s=2,v=2;{p64}\x1b\\");
            let segs = it.feed(seq.as_bytes());
            assert!(
                only_images(segs.clone()).is_empty(),
                "t={medium} must produce no image (no file read)"
            );
            // And it is SUPPRESSED from the local terminal (empty tee) — so a
            // detect-mode client's fallback resend won't double-render — and a
            // kitty error is injected to the app to trigger that fallback.
            assert!(
                tee_bytes(&segs).is_empty(),
                "t={medium} must not reach the local terminal"
            );
            assert_eq!(rejections(&segs).len(), 1, "t={medium} rejected to the app");
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn kitty_file_query_echoes_id_for_fallback() {
        // A detect-mode client tests file/shm support with an `a=q` query carrying
        // an image id. We must answer with a kitty error ECHOING that id so the
        // client correlates it, concludes the medium is unsupported, and falls back
        // to direct transmission (which we mirror). The query path never touches
        // the filesystem (payload is a bogus name — irrelevant, we don't read it).
        let mut it = Interceptor::new();
        let name = B64.encode("/some/shm-name");
        let segs = it.feed(format!("\x1b_Gi=31,s=1,v=1,a=q,t=s;{name}\x1b\\").as_bytes());
        let rej = rejections(&segs);
        assert_eq!(rej.len(), 1);
        let resp = String::from_utf8(rej[0].clone()).unwrap();
        assert!(
            resp.starts_with("\x1b_Gi=31;E"),
            "echoes id, error code: {resp:?}"
        );
        assert!(resp.ends_with("\x1b\\"));
        assert!(
            tee_bytes(&segs).is_empty(),
            "query suppressed from terminal"
        );
        assert!(only_images(segs).is_empty());
    }

    #[test]
    fn kitty_direct_still_mirrors_and_tees() {
        // The allowed path is unchanged: a direct image is mirrored AND its original
        // bytes tee to the local terminal (which renders it natively).
        let mut it = Interceptor::new();
        let s = format!("\x1b_Ga=T,f=100,t=d;{}\x1b\\", png_b64());
        let segs = it.feed(s.as_bytes());
        assert_eq!(only_images(segs.clone()).len(), 1, "mirrored");
        assert_eq!(
            tee_bytes(&segs),
            s.as_bytes(),
            "teed verbatim to the terminal"
        );
        assert!(rejections(&segs).is_empty());
    }

    #[test]
    fn oversized_direct_payload_dropped_on_every_path() {
        // An image the wire could never carry (full frame > MAX_WS_MESSAGE would
        // wedge push mode in a reconnect loop) must be dropped, not forwarded —
        // for kitty direct, single-shot iTerm2, and multipart iTerm2 alike.
        let big = "A".repeat(MAX_B64_BYTES + 4);
        for seq in [
            format!("\x1b_Ga=T,f=100,t=d;{big}\x1b\\"),
            format!("\x1b]1337;File=inline=1:{big}\x07"),
        ] {
            let mut it = Interceptor::new();
            let mut segs = Vec::new();
            for chunk in seq.as_bytes().chunks(1 << 20) {
                segs.extend(it.feed(chunk));
            }
            assert!(only_images(segs).is_empty(), "oversized image forwarded");
        }
        let mut it = Interceptor::new();
        let mut segs = it.feed(b"\x1b]1337;MultipartFile=inline=1\x07");
        for part in big.as_bytes().chunks(1 << 20) {
            let seq = format!(
                "\x1b]1337;FilePart={}\x07",
                std::str::from_utf8(part).unwrap()
            );
            segs.extend(it.feed(seq.as_bytes()));
        }
        segs.extend(it.feed(b"\x1b]1337;FileEnd\x07"));
        assert!(
            only_images(segs).is_empty(),
            "oversized multipart forwarded"
        );
    }

    #[test]
    fn zlib_bomb_dropped_after_inflate() {
        // o=z: a tiny payload inflating past MAX_IMAGE_BYTES must die at the
        // post-inflate check, before PNG encoding buffers it for the wire.
        use std::io::Write;
        let raw = vec![0u8; (2100 * 2100 * 4) as usize]; // ~17.6 MB of zeros
        let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(&raw).unwrap();
        let bomb = B64.encode(enc.finish().unwrap());
        assert!(
            bomb.len() < MAX_B64_BYTES,
            "the *transmitted* payload is small"
        );
        let mut it = Interceptor::new();
        let s = format!("\x1b_Ga=T,f=32,o=z,t=d,s=2100,v=2100;{bomb}\x1b\\");
        let mut segs = Vec::new();
        for chunk in s.as_bytes().chunks(1 << 20) {
            segs.extend(it.feed(chunk));
        }
        assert!(only_images(segs).is_empty());
    }

    #[test]
    fn zero_cell_hints_fall_back_to_natural_size() {
        // width=0 / c=0 must read as "unspecified", not a zero-width image —
        // both protocols define 0 as "auto".
        let b64 = png_b64();
        let mut it = Interceptor::new();
        let s = format!("\x1b]1337;File=inline=1;width=0;height=0:{b64}\x07");
        let imgs = only_images(it.feed(s.as_bytes()));
        assert_eq!(imgs.len(), 1);
        assert_eq!(imgs[0].place.cells, None, "zero hint ⇒ natural size");
        let mut it = Interceptor::new();
        let s = format!("\x1b_Ga=T,f=100,t=d,c=0,r=0;{b64}\x1b\\");
        let imgs = only_images(it.feed(s.as_bytes()));
        assert_eq!(imgs.len(), 1);
        assert_eq!(imgs[0].place.cells, None);
    }

    #[test]
    fn non_image_escapes_pass_through_untouched() {
        // The interceptor only extracts images; a clear/alt-screen sequence is not
        // its concern (image eviction rides the grid's per-cell tags now), so it
        // must pass straight through to vt100.
        let mut it = Interceptor::new();
        let passed = only_pass(&it.feed(b"abc\x1b[2Jdef"));
        assert_eq!(passed, b"abc\x1b[2Jdef");
    }
}

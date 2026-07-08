//! Inline-image interceptor: pull iTerm2 (OSC 1337) and kitty (APC `_G`) image
//! sequences out of the raw PTY byte stream so they can be forwarded to the browser
//! as `<img>` overlays. vt100 drops these sequences entirely (it implements no
//! DCS/APC/OSC-1337 handler), so the images would otherwise be lost.
//!
//! We only *extract* image sequences; every other byte passes straight through as
//! [`Segment::Pass`] to vt100, which is a streaming parser and reassembles partial
//! escape sequences across calls on its own. So the only thing this scanner has to
//! reassemble across PTY read boundaries is an image sequence itself.
//!
//! Handled: iTerm2 OSC 1337 `File` — single-shot or multipart
//! (`MultipartFile`/`FilePart`/`FileEnd`); kitty `_G` in
//! PNG (`f=100`) or raw RGB/RGBA (`f=24`/`f=32`, re-encoded to PNG) over any of the
//! direct (`t=d`), file (`t=f`/`t=t`), or shared-memory (`t=s`) transmission
//! mediums; and sixel DCS (`ESC P … q … ST`, decoded to RGBA and re-encoded to
//! PNG). For the file/shm mediums the payload is a path/name, so we read the same
//! bytes the real terminal reads — read-only, since the terminal keeps rendering
//! locally and owns any cleanup, so we never race its delete.

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

/// One extracted inline image, ready to hand the browser as a `data:` URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    /// MIME type sniffed from the decoded bytes (`image/png`, …).
    pub mime: String,
    /// The image file, base64 (forwarded verbatim; the browser decodes it).
    pub base64: String,
    /// Display size in terminal cells (cols, rows) if the app specified one;
    /// otherwise the browser renders the image at its natural pixel size.
    pub cells: Option<(u16, u16)>,
    /// Source pixel dimensions (width, height). Used to derive a cell count when
    /// the app gave none, so the cursor advances below a natural-size image.
    pub px: Option<(u32, u32)>,
}

/// Pixel dimensions from an encoded image's header (any format the browser takes).
fn dims(bytes: &[u8]) -> Option<(u32, u32)> {
    let s = imagesize::blob_size(bytes).ok()?;
    Some((s.width as u32, s.height as u32))
}

/// What the interceptor emits for a run of input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    /// Non-image bytes — feed to vt100 (and tee to the local terminal).
    Pass(Vec<u8>),
    /// A fully-received inline image, to place at the current cursor cell.
    Image(Image),
}

const ITERM: &[u8] = b"\x1b]1337;";
const KITTY: &[u8] = b"\x1b_G";
/// DCS introducer. Sixel is `ESC P <params> q …`; other DCS strings pass through.
const DCS: &[u8] = b"\x1bP";

/// Streaming extractor. Owns only the bytes of an in-flight image sequence plus a
/// tiny carry for a start marker split across a read boundary.
#[derive(Default)]
pub struct Interceptor {
    /// Bytes of a sequence still awaiting its terminator (spans reads).
    seq: Vec<u8>,
    /// Concatenated kitty base64 payload across `m=1` chunks, with the display-cell
    /// hint from the first chunk.
    kitty: Option<KittyAccum>,
    /// In-flight iTerm2 multipart transfer (`MultipartFile`→`FilePart`*→`FileEnd`).
    iterm: Option<ItermAccum>,
    /// Intercept kitty / iTerm2 / sixel sequences. A protocol the local terminal
    /// doesn't render is left in the stream (vt100 sees it, matching the terminal)
    /// rather than consumed into a web image the local terminal wouldn't show — see
    /// the capability handshake in `pty.rs`.
    do_kitty: bool,
    do_iterm: bool,
    do_sixel: bool,
}

/// Concatenated iTerm2 `FilePart` base64 across a multipart transfer, with the
/// display-cell hint from the opening `MultipartFile`.
struct ItermAccum {
    payload: String,
    cells: Option<(u16, u16)>,
}

struct KittyAccum {
    payload: String,
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

/// The kitty pixel formats we can turn into something the browser renders.
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

/// How the transmitted bytes reach us — the kitty `t` key. For the indirect
/// mediums the base64 payload is a *reference* (path / shm name), not the image,
/// so we read the referenced bytes ourselves — the same bytes the real terminal
/// reads. Read-only: the terminal keeps rendering locally and owns any cleanup, so
/// we never race its delete.
enum KittyMedium {
    /// `t=d` (or absent): the payload is the base64 image itself.
    Direct,
    /// `t=f`/`t=t`: the payload is a base64 filesystem path (`t=t` also asks the
    /// terminal to delete after reading; we leave that to the real terminal).
    File,
    /// `t=s`: the payload is a base64 POSIX shared-memory object name.
    Shm,
    /// Unknown transmission medium — drop.
    Unsupported,
}

impl Interceptor {
    /// Intercept all protocols — for tests and when capabilities are unknown.
    #[cfg(test)]
    pub fn new() -> Self {
        Self::with(true, true, true)
    }

    /// Intercept only the protocols the terminal supports (per the handshake).
    pub fn with(do_kitty: bool, do_iterm: bool, do_sixel: bool) -> Self {
        Self {
            do_kitty,
            do_iterm,
            do_sixel,
            ..Default::default()
        }
    }

    /// True if `s` is a nonempty proper prefix of an *enabled* start marker — a
    /// marker split across the read boundary, carried until the rest arrives.
    fn split_prefix(&self, s: &[u8]) -> bool {
        !s.is_empty()
            && ((self.do_iterm && s.len() < ITERM.len() && ITERM.starts_with(s))
                || (self.do_kitty && s.len() < KITTY.len() && KITTY.starts_with(s))
                || (self.do_sixel && s.len() < DCS.len() && DCS.starts_with(s)))
    }

    /// Feed one PTY read; returns the segments to apply in order.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<Segment> {
        // Prepend any carried partial (either an in-flight sequence or a split
        // marker prefix). `seq` empty ⇒ we're scanning plain bytes.
        let data: Vec<u8> = if self.seq.is_empty() {
            chunk.to_vec()
        } else {
            let mut v = std::mem::take(&mut self.seq);
            v.extend_from_slice(chunk);
            v
        };

        let mut out = Vec::new();
        let mut pass_start = 0usize;
        let mut i = 0usize;

        while i < data.len() {
            // Fast path: only ESC can begin something we care about.
            if data[i] != 0x1b {
                i += 1;
                continue;
            }
            let rest = &data[i..];
            let kind = if self.do_iterm && rest.starts_with(ITERM) {
                Some(Marker::Iterm)
            } else if self.do_kitty && rest.starts_with(KITTY) {
                Some(Marker::Kitty)
            } else if self.do_sixel && rest.starts_with(DCS) {
                match sixel_dcs(rest) {
                    Some(true) => Some(Marker::Sixel),
                    Some(false) => {
                        i += 1; // a non-sixel DCS — leave it for vt100
                        continue;
                    }
                    None => {
                        // DCS header split across the read boundary — carry the tail.
                        push_pass(&mut out, &data[pass_start..i]);
                        self.seq = rest.to_vec();
                        return out;
                    }
                }
            } else if self.split_prefix(rest) {
                // Possible marker split across the read boundary — carry the tail.
                push_pass(&mut out, &data[pass_start..i]);
                self.seq = rest.to_vec();
                return out;
            } else {
                i += 1;
                continue;
            };

            let marker = kind.unwrap();
            // Find the sequence terminator (BEL or ST) after the marker.
            match find_terminator(&data[i..], marker) {
                Some(end) => {
                    // Whole sequence is `data[i .. i+end]`.
                    push_pass(&mut out, &data[pass_start..i]);
                    let seq = &data[i..i + end];
                    if let Some(seg) = self.parse_sequence(marker, seq) {
                        out.push(seg);
                    }
                    i += end;
                    pass_start = i;
                }
                None => {
                    // Terminator not in this read — carry the whole partial sequence.
                    push_pass(&mut out, &data[pass_start..i]);
                    self.seq = data[i..].to_vec();
                    return out;
                }
            }
        }
        push_pass(&mut out, &data[pass_start..]);
        out
    }

    /// Parse a complete image sequence (marker .. terminator inclusive).
    fn parse_sequence(&mut self, marker: Marker, seq: &[u8]) -> Option<Segment> {
        match marker {
            Marker::Iterm => self.parse_iterm(seq),
            Marker::Kitty => self.parse_kitty(seq),
            Marker::Sixel => parse_sixel(seq),
        }
    }

    /// Handle one iTerm2 `\x1b]1337;` sequence: a single-shot `File=` image, or one
    /// verb of a multipart transfer (`MultipartFile=` opens, `FilePart=` appends,
    /// `FileEnd` flushes).
    fn parse_iterm(&mut self, seq: &[u8]) -> Option<Segment> {
        let body = strip_terminator(&seq[ITERM.len()..]);
        if let Some(args) = body.strip_prefix(b"File=") {
            return iterm_single(args).map(Segment::Image);
        }
        if let Some(args) = body.strip_prefix(b"MultipartFile=") {
            let kv = parse_kv(args, b';');
            self.iterm = Some(ItermAccum {
                payload: String::new(),
                cells: cells_from(kv.get("width"), kv.get("height")),
            });
            return None;
        }
        if let Some(part) = body.strip_prefix(b"FilePart=") {
            if let Some(acc) = self.iterm.as_mut() {
                acc.payload
                    .push_str(std::str::from_utf8(part).unwrap_or("").trim());
            }
            return None;
        }
        if body.starts_with(b"FileEnd") {
            let acc = self.iterm.take()?;
            return image_from_b64(acc.payload, acc.cells).map(Segment::Image);
        }
        None
    }

    /// Handle one kitty `_G` sequence, accumulating `m=1` chunks.
    fn parse_kitty(&mut self, seq: &[u8]) -> Option<Segment> {
        // Strip `\x1b_G` prefix and `\x1b\\` suffix.
        let body = &seq[KITTY.len()..seq.len().saturating_sub(2)];
        let (control, payload) = match body.iter().position(|&b| b == b';') {
            Some(p) => (&body[..p], &body[p + 1..]),
            None => (body, &b""[..]),
        };
        let ctrl = parse_kv(control, b',');
        let more = ctrl.get("m").map(|v| v == "1").unwrap_or(false);

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
            acc.payload
                .push_str(std::str::from_utf8(payload).unwrap_or(""));
        }
        if more {
            return None; // wait for the rest
        }
        // Last chunk — flush. Resolve the medium to the image bytes, then render.
        let acc = self.kitty.take()?;
        if matches!(acc.fmt, KittyFmt::Unsupported)
            || matches!(acc.medium, KittyMedium::Unsupported)
        {
            return None;
        }
        // The base64 payload is either the image itself (direct) or a reference
        // (path / shm name) we resolve to the same bytes the real terminal reads.
        let decoded = B64.decode(acc.payload.trim()).ok()?;
        let mut bytes = match acc.medium {
            KittyMedium::Direct => decoded,
            KittyMedium::File => read_capped(std::str::from_utf8(&decoded).ok()?)?,
            KittyMedium::Shm => read_capped(&shm_path(std::str::from_utf8(&decoded).ok()?))?,
            KittyMedium::Unsupported => return None,
        };
        // `S`/`O`: the transmitted data may be a window into the file/shm object.
        if let Some(sz) = acc.size {
            bytes = bytes.get(acc.offset..acc.offset.checked_add(sz)?)?.to_vec();
        }
        if acc.zlib {
            bytes = zlib_inflate(&bytes)?;
        }
        let image = match acc.fmt {
            KittyFmt::Png => Image {
                mime: sniff_mime(&bytes)?.to_string(),
                base64: B64.encode(&bytes),
                cells: acc.cells,
                px: dims(&bytes),
            },
            KittyFmt::Rgba | KittyFmt::Rgb => {
                let (w, h) = acc.px?;
                let channels = if matches!(acc.fmt, KittyFmt::Rgba) {
                    4
                } else {
                    3
                };
                let png = encode_png(w, h, channels, &bytes)?;
                Image {
                    mime: "image/png".to_string(),
                    base64: B64.encode(&png),
                    cells: acc.cells,
                    px: Some((w, h)),
                }
            }
            KittyFmt::Unsupported => return None,
        };
        Some(Segment::Image(image))
    }
}

/// Read a file referenced by a kitty file/shm transmission, capped so a stray or
/// hostile reference can't pull an unbounded blob into memory and onto the wire.
/// The format sniff downstream (`sniff_mime`) then drops anything that isn't an
/// image, so only genuine image files ever reach a viewer.
// ponytail: no path allowlist — we read whatever the app referenced, matching the
// terminal we mirror; the size cap + mime sniff are the backstop. Revisit if
// broadcasting to a hub makes arbitrary-file reads a concern worth restricting.
fn read_capped(path: &str) -> Option<Vec<u8>> {
    const MAX_IMAGE_BYTES: u64 = 16 << 20;
    if std::fs::metadata(path).ok()?.len() > MAX_IMAGE_BYTES {
        return None;
    }
    std::fs::read(path).ok()
}

/// Map a POSIX shared-memory object name (kitty `t=s`) to its Linux `/dev/shm`
/// path. A leading `/` is part of the shm namespace, not a real path component.
// ponytail: Linux `/dev/shm`; use `shm_open` if a non-Linux Unix ever needs this.
fn shm_path(name: &str) -> String {
    format!("/dev/shm/{}", name.strip_prefix('/').unwrap_or(name))
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
fn encode_png(width: u32, height: u32, channels: u8, pixels: &[u8]) -> Option<Vec<u8>> {
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

/// Decode a sixel DCS (`ESC P … q … ST`) to RGBA (via `icy_sixel`), then re-encode
/// as PNG the browser can render. Sixel carries no cell size, so `pty.rs` derives
/// one from the pixel dimensions.
fn parse_sixel(seq: &[u8]) -> Option<Segment> {
    let img = icy_sixel::SixelImage::decode(seq).ok()?;
    let (w, h) = (
        u32::try_from(img.width).ok()?,
        u32::try_from(img.height).ok()?,
    );
    let png = encode_png(w, h, 4, &img.pixels)?;
    Some(Segment::Image(Image {
        mime: "image/png".to_string(),
        base64: B64.encode(&png),
        cells: None,
        px: Some((w, h)),
    }))
}

/// A single-shot iTerm2 `File=<k=v>;…:<base64>` payload (verb and terminator
/// already stripped): `args` is `name=…;width=…;height=…;inline=1:<base64>`.
fn iterm_single(args: &[u8]) -> Option<Image> {
    // Split params from base64 at the first ':'.
    let colon = args.iter().position(|&b| b == b':')?;
    let params = &args[..colon];
    let b64 = std::str::from_utf8(&args[colon + 1..]).ok()?.trim();
    let kv = parse_kv(params, b';');
    let cells = cells_from(kv.get("width"), kv.get("height"));
    image_from_b64(b64.to_string(), cells)
}

/// Decode base64, sniff the format, and package it — or `None` if it isn't a
/// browser-native image.
fn image_from_b64(base64: String, cells: Option<(u16, u16)>) -> Option<Image> {
    let bytes = B64.decode(base64.trim()).ok()?;
    let mime = sniff_mime(&bytes)?;
    Some(Image {
        mime: mime.to_string(),
        base64: base64.trim().to_string(),
        cells,
        px: dims(&bytes),
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

/// A cols/rows display hint, only when both are plain integers (cells) — iTerm2's
/// `Npx`/`N%`/`auto` and kitty pixel sizing fall back to natural size.
fn cells_from(c: Option<&String>, r: Option<&String>) -> Option<(u16, u16)> {
    let c = c?.parse::<u16>().ok()?;
    let r = r?.parse::<u16>().ok()?;
    Some((c, r))
}

/// Find the end (one past the terminator) of an image sequence starting at `s[0]`.
/// iTerm2 ends at BEL or ST; kitty and sixel end at ST (`ESC \`, or 8-bit `0x9c`).
fn find_terminator(s: &[u8], marker: Marker) -> Option<usize> {
    let mut i = 1; // skip the leading ESC so we don't match it as an ST
    while i < s.len() {
        match s[i] {
            0x07 if matches!(marker, Marker::Iterm) => return Some(i + 1), // BEL
            0x9c if matches!(marker, Marker::Sixel) => return Some(i + 1), // 8-bit ST
            0x1b if i + 1 < s.len() && s[i + 1] == b'\\' => return Some(i + 2), // ST
            0x1b if i + 1 == s.len() => return None, // ESC at very end — need more
            _ => i += 1,
        }
    }
    None
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

fn push_pass(out: &mut Vec<Segment>, bytes: &[u8]) {
    if !bytes.is_empty() {
        out.push(Segment::Pass(bytes.to_vec()));
    }
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

    fn only_images(segs: Vec<Segment>) -> Vec<Image> {
        segs.into_iter()
            .filter_map(|s| match s {
                Segment::Image(i) => Some(i),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn iterm_inline_image_extracted_with_passthrough() {
        let mut it = Interceptor::new();
        let b64 = png_b64();
        let stream = format!("hi\x1b]1337;File=inline=1;width=4;height=2:{b64}\x07bye");
        let segs = it.feed(stream.as_bytes());

        // Text before and after passes through; image pulled out with size hint.
        let passed: Vec<u8> = segs
            .iter()
            .filter_map(|s| match s {
                Segment::Pass(b) => Some(b.clone()),
                _ => None,
            })
            .flatten()
            .collect();
        assert_eq!(passed, b"hibye");
        let imgs = only_images(segs);
        assert_eq!(imgs.len(), 1);
        assert_eq!(imgs[0].mime, "image/png");
        assert_eq!(imgs[0].cells, Some((4, 2)));
        assert_eq!(imgs[0].base64, b64);
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
        assert_eq!(imgs[0].cells, Some((4, 2)));
        assert_eq!(imgs[0].base64, b64);
    }

    #[test]
    fn gated_off_protocol_passes_through() {
        // Kitty + sixel off (terminal doesn't support them): those sequences are not
        // consumed — they pass through to vt100, matching what the terminal shows.
        let mut it = Interceptor::with(false, true, false);
        let s = format!("\x1b_Ga=T,f=100,t=d;{}\x1b\\", png_b64());
        let sixel = icy_sixel::SixelImage::from_rgba(vec![255, 0, 0, 255], 1, 1)
            .encode()
            .unwrap();
        let stream = format!("{s}{sixel}");
        let segs = it.feed(stream.as_bytes());
        assert!(only_images(segs.clone()).is_empty());
        let passed: Vec<u8> = segs
            .into_iter()
            .filter_map(|seg| match seg {
                Segment::Pass(b) => Some(b),
                Segment::Image(_) => None,
            })
            .flatten()
            .collect();
        assert_eq!(
            passed,
            stream.as_bytes(),
            "gated-off bytes reach vt100 verbatim"
        );
        // iTerm2 is still intercepted (its flag is on).
        let mut it2 = Interceptor::with(false, true, false);
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
        assert_eq!(imgs[0].cells, None);
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
        assert_eq!(imgs[0].cells, Some((3, 1)));
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

        let png = B64.decode(&imgs[0].base64).unwrap();
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
    fn kitty_file_transport_reads_the_referenced_png() {
        // t=f: the payload is a base64 path; we read that PNG file (the same bytes
        // the real terminal reads) and forward it like a direct image. The path is
        // base64'd *without* padding, exactly as `kitten icat` emits it — the decode
        // must tolerate that (the stock STANDARD engine would reject it).
        let path = std::env::temp_dir().join("sg_icat_file_test.png");
        std::fs::write(&path, B64.decode(png_b64()).unwrap()).unwrap();
        let b64path = B64.encode(path.to_str().unwrap().as_bytes());
        let unpadded = b64path.trim_end_matches('=');
        let mut it = Interceptor::new();
        let s = format!("\x1b_Ga=T,f=100,t=f;{unpadded}\x1b\\");
        let imgs = only_images(it.feed(s.as_bytes()));
        let _ = std::fs::remove_file(&path);
        assert_eq!(imgs.len(), 1);
        assert_eq!(imgs[0].mime, "image/png");
        assert_eq!(imgs[0].px, Some((1, 1))); // png_b64 is 1x1
    }

    #[test]
    fn kitty_missing_file_and_unknown_medium_dropped() {
        let mut it = Interceptor::new();
        // t=f pointing at a nonexistent path → nothing to show.
        let noent = B64.encode(b"/nonexistent/sg/does-not-exist.png");
        let s = format!("\x1b_Ga=T,f=100,t=f;{noent}\x1b\\");
        assert!(only_images(it.feed(s.as_bytes())).is_empty());
        // An unknown transmission medium is dropped.
        let s = format!("\x1b_Ga=T,f=100,t=x;{}\x1b\\", png_b64());
        assert!(only_images(it.feed(s.as_bytes())).is_empty());
    }

    #[test]
    fn non_image_escapes_pass_through_untouched() {
        // The interceptor only extracts images; a clear/alt-screen sequence is not
        // its concern (image eviction rides the grid sentinel now), so it must pass
        // straight through to vt100.
        let mut it = Interceptor::new();
        let passed: Vec<u8> = it
            .feed(b"abc\x1b[2Jdef")
            .into_iter()
            .filter_map(|s| match s {
                Segment::Pass(b) => Some(b),
                Segment::Image(_) => None,
            })
            .flatten()
            .collect();
        assert_eq!(passed, b"abc\x1b[2Jdef");
    }
}

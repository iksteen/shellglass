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
//! MVP scope (see exp/inline-images): direct base64 payloads in a browser-native
//! image format (PNG/JPEG/GIF/WebP) only.
//! - kitty: `t=d` (direct) transfers with `f=100` (PNG) — file/shared-mem mediums
//!   and raw-pixel formats (`f=24/32`) are skipped, since the browser can't render
//!   a bare pixel buffer and we won't read the sender's local files.
//! - iTerm2 payloads are always a direct base64 image file, so all are handled.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;

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
}

/// What the interceptor emits for a run of input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    /// Non-image bytes — feed to vt100 (and tee to the local terminal).
    Pass(Vec<u8>),
    /// A fully-received inline image, to place at the current cursor cell.
    Image(Image),
    /// A screen clear / alt-screen toggle — drop all currently-placed images.
    ClearImages,
}

const ITERM: &[u8] = b"\x1b]1337;";
const KITTY: &[u8] = b"\x1b_G";
/// Longest start marker, for the split-across-reads carry.
const MAX_MARKER: usize = ITERM.len();

/// Streaming extractor. Owns only the bytes of an in-flight image sequence plus a
/// tiny carry for a start marker split across a read boundary.
#[derive(Default)]
pub struct Interceptor {
    /// Bytes of a sequence still awaiting its terminator (spans reads).
    seq: Vec<u8>,
    /// Concatenated kitty base64 payload across `m=1` chunks, with the display-cell
    /// hint from the first chunk.
    kitty: Option<KittyAccum>,
}

struct KittyAccum {
    payload: String,
    cells: Option<(u16, u16)>,
    fmt: KittyFmt,
    /// Source pixel dimensions (kitty `s`×`v`), needed to encode raw formats.
    px: Option<(u32, u32)>,
    /// `o=z`: the payload is zlib-compressed.
    zlib: bool,
}

/// The kitty pixel formats we can turn into something the browser renders.
enum KittyFmt {
    /// `f=100`: already an encoded image file — forward as-is.
    Png,
    /// `f=32`: raw RGBA pixels — re-encode as PNG.
    Rgba,
    /// `f=24`: raw RGB pixels — re-encode as PNG.
    Rgb,
    /// Unknown format or non-direct transfer medium — drop.
    Unsupported,
}

impl Interceptor {
    pub fn new() -> Self {
        Self::default()
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
            let kind = if rest.starts_with(ITERM) {
                Some(Marker::Iterm)
            } else if rest.starts_with(KITTY) {
                Some(Marker::Kitty)
            } else if rest.len() < MAX_MARKER && is_marker_prefix(rest) {
                // Possible marker split across the read boundary — carry the tail.
                push_pass(&mut out, &data[pass_start..i]);
                self.seq = rest.to_vec();
                return out;
            } else if let Some(len) = clear_len(rest) {
                // Screen clear / alt-screen toggle: pass it through *and* drop images.
                push_pass(&mut out, &data[pass_start..i + len]);
                out.push(Segment::ClearImages);
                i += len;
                pass_start = i;
                continue;
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
            Marker::Iterm => parse_iterm(seq).map(Segment::Image),
            Marker::Kitty => self.parse_kitty(seq),
        }
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
            let direct = ctrl.get("t").map(|t| t == "d").unwrap_or(true); // default d=direct
            let fmt = match ctrl.get("f").map(String::as_str) {
                Some("100") => KittyFmt::Png,
                Some("32") | None => KittyFmt::Rgba, // kitty's default format is 32
                Some("24") => KittyFmt::Rgb,
                _ => KittyFmt::Unsupported,
            };
            self.kitty = Some(KittyAccum {
                payload: String::new(),
                cells: cells_from(ctrl.get("c"), ctrl.get("r")),
                fmt: if direct { fmt } else { KittyFmt::Unsupported },
                px: cells_from(ctrl.get("s"), ctrl.get("v"))
                    .map(|(w, h)| (u32::from(w), u32::from(h))),
                zlib: ctrl.get("o").map(|o| o == "z").unwrap_or(false),
            });
        }
        if let Some(acc) = self.kitty.as_mut() {
            acc.payload
                .push_str(std::str::from_utf8(payload).unwrap_or(""));
        }
        if more {
            return None; // wait for the rest
        }
        // Last chunk — flush. Decode base64 (and zlib, if o=z), then render per format.
        let acc = self.kitty.take()?;
        if matches!(acc.fmt, KittyFmt::Unsupported) {
            return None;
        }
        let raw = B64.decode(acc.payload.trim()).ok()?;
        let bytes = if acc.zlib { zlib_inflate(&raw)? } else { raw };
        let image = match acc.fmt {
            KittyFmt::Png => Image {
                mime: sniff_mime(&bytes)?.to_string(),
                base64: B64.encode(&bytes),
                cells: acc.cells,
            },
            KittyFmt::Rgba | KittyFmt::Rgb => {
                let (w, h) = acc.px?;
                let channels = if matches!(acc.fmt, KittyFmt::Rgba) { 4 } else { 3 };
                let png = encode_png(w, h, channels, &bytes)?;
                Image {
                    mime: "image/png".to_string(),
                    base64: B64.encode(&png),
                    cells: acc.cells,
                }
            }
            KittyFmt::Unsupported => return None,
        };
        Some(Segment::Image(image))
    }
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
}

/// iTerm2 `\x1b]1337;File=<k=v>;…:<base64><term>`.
fn parse_iterm(seq: &[u8]) -> Option<Image> {
    // Body between the marker and the terminator (BEL or ST).
    let body = strip_terminator(&seq[ITERM.len()..]);
    // Split params from base64 at the first ':'.
    let colon = body.iter().position(|&b| b == b':')?;
    let params = &body[..colon];
    let b64 = std::str::from_utf8(&body[colon + 1..]).ok()?.trim();
    // Params look like `File=name=…;width=…;height=…;inline=1`.
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

/// Screen-clear sequences whose presence should evict all placed images: `ESC[2J`,
/// `ESC[3J`, and the alt-screen toggles. Returns the byte length if `rest` starts
/// with one.
fn clear_len(rest: &[u8]) -> Option<usize> {
    const CLEARS: &[&[u8]] = &[
        b"\x1b[2J",
        b"\x1b[3J",
        b"\x1b[?1049h",
        b"\x1b[?1049l",
        b"\x1b[?1047h",
        b"\x1b[?1047l",
        b"\x1b[?47h",
        b"\x1b[?47l",
    ];
    CLEARS
        .iter()
        .find(|c| rest.starts_with(c))
        .map(|c| c.len())
}

/// Find the end (one past the terminator) of an image sequence starting at `s[0]`.
/// iTerm2 ends at BEL or ST; kitty ends at ST (`ESC \`).
fn find_terminator(s: &[u8], marker: Marker) -> Option<usize> {
    let mut i = 1; // skip the leading ESC so we don't match it as an ST
    while i < s.len() {
        match s[i] {
            0x07 if matches!(marker, Marker::Iterm) => return Some(i + 1), // BEL
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

/// True if `s` is a nonempty prefix of one of the start markers (for the
/// split-across-reads carry).
fn is_marker_prefix(s: &[u8]) -> bool {
    (!s.is_empty()) && (ITERM.starts_with(s) || KITTY.starts_with(s))
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
        let s = format!("\x1b_Ga=T,f=32,t=d,s=2,v=2,c=1,r=1;{}\x1b\\", B64.encode(src));
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
        assert_eq!(&buf[..frame.buffer_size()], &src[..], "pixels round-trip exactly");
    }

    #[test]
    fn kitty_nondirect_transfer_skipped() {
        let mut it = Interceptor::new();
        // t=f (file transfer) — we won't read the sender's local files; must drop.
        let s = format!("\x1b_Ga=T,f=100,t=f;{}\x1b\\", png_b64());
        assert!(only_images(it.feed(s.as_bytes())).is_empty());
    }

    #[test]
    fn clear_sequences_emit_clearimages() {
        let mut it = Interceptor::new();
        let segs = it.feed(b"abc\x1b[2Jdef");
        assert!(segs.contains(&Segment::ClearImages));
        assert!(segs.contains(&Segment::Pass(b"def".to_vec())));
    }
}

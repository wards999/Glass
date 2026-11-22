//! Clipboard sharing for a session (M5c, step 1): text and images, both
//! ways. A helper thread watches this PC's clipboard (its sequence number,
//! every 100 ms), reads and PNG-encodes a new item and leaves it in a slot;
//! the session loop (host) or the viewer's UI timer takes it from there and
//! sends it, and hands over what arrived from the other side, which the
//! thread puts on the clipboard. So the hot paths only swap an item through
//! a slot: the reading, encoding and decoding all happen here.
//!
//! Text travels as UTF-8 with `\n` line endings; images as PNG (the
//! registered "PNG" format if the copying app offered it, else the DIB
//! converted). Both "PNG" and a DIB are put on the clipboard, so every app
//! can paste.

use glass_core::clipboard::{ClipItem, CLIP_MAX, CLIP_PNG, CLIP_TEXT};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use windows::core::w;
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber,
    IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
};

const CF_DIB: u32 = 8;
const CF_UNICODETEXT: u32 = 13;
/// The largest image converted from a DIB (pixels).
const MAX_PIXELS: usize = 8192 * 8192;

#[derive(Default)]
struct Shared {
    stop: AtomicBool,
    /// Read from this PC's clipboard, waiting to be sent.
    out: Mutex<Option<ClipItem>>,
    /// From the other side, waiting to go on the clipboard.
    inbox: Mutex<Option<ClipItem>>,
}

/// The clipboard thread for one session; it stops when this is dropped.
pub struct ClipSync(Arc<Shared>);

impl ClipSync {
    /// `send_current`: also send what's on the clipboard now (the viewer
    /// does, so something copied before connecting can be pasted there).
    pub fn start(send_current: bool) -> Self {
        let sh = Arc::new(Shared::default());
        let t = sh.clone();
        let _ = std::thread::Builder::new()
            .name("glass-clipboard".into())
            .spawn(move || watch(&t, send_current));
        Self(sh)
    }

    /// A new item from this PC's clipboard, to send. (The thread holds
    /// either slot's lock only to move an item in or out.)
    pub fn take_outgoing(&self) -> Option<ClipItem> {
        self.0.out.lock().ok()?.take()
    }

    /// An item from the other side, to put on the clipboard.
    pub fn put_incoming(&self, item: ClipItem) {
        if let Ok(mut s) = self.0.inbox.lock() {
            *s = Some(item);
        }
    }
}

impl Drop for ClipSync {
    fn drop(&mut self) {
        self.0.stop.store(true, Ordering::Relaxed);
    }
}

fn hash(item: &ClipItem) -> u64 {
    let mut h = DefaultHasher::new();
    item.hash(&mut h);
    h.finish()
}

fn watch(sh: &Shared, send_current: bool) {
    let png_format = unsafe { RegisterClipboardFormatW(w!("PNG")) };
    let mut seen = if send_current {
        0
    } else {
        unsafe { GetClipboardSequenceNumber() }
    };
    // The last item sent or put on the clipboard: an app that sets the same
    // contents again isn't news, and what arrived isn't sent back.
    let mut last = 0u64;
    while !sh.stop.load(Ordering::Relaxed) {
        let incoming = sh.inbox.lock().ok().and_then(|mut s| s.take());
        if let Some(item) = incoming {
            if set(&item, png_format) {
                last = hash(&item);
                seen = unsafe { GetClipboardSequenceNumber() };
            }
        }
        let now = unsafe { GetClipboardSequenceNumber() };
        if now != seen {
            // Another app may hold the clipboard open: try again next time.
            if let Some(item) = read(png_format) {
                seen = now;
                let h = hash(&item);
                if h != last {
                    last = h;
                    if let Ok(mut s) = sh.out.lock() {
                        *s = Some(item);
                    }
                }
            } else if !busy() {
                seen = now; // nothing we share (files, empty)
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Opens the clipboard, retrying briefly (other apps hold it for a moment).
fn open() -> bool {
    for _ in 0..5 {
        if unsafe { OpenClipboard(None) }.is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

fn busy() -> bool {
    if open() {
        unsafe {
            let _ = CloseClipboard();
        }
        false
    } else {
        true
    }
}

/// Copies a clipboard handle's bytes.
unsafe fn bytes(h: HANDLE) -> Option<Vec<u8>> {
    let g = HGLOBAL(h.0);
    let n = GlobalSize(g);
    let p = GlobalLock(g) as *const u8;
    if p.is_null() || n == 0 || n > CLIP_MAX + 4096 {
        let _ = GlobalUnlock(g);
        return None;
    }
    let v = std::slice::from_raw_parts(p, n).to_vec();
    let _ = GlobalUnlock(g);
    Some(v)
}

/// What's on the clipboard now, if it's something we share: text first
/// (a spreadsheet's cells also come as a picture), then an image.
fn read(png_format: u32) -> Option<ClipItem> {
    if !open() {
        return None;
    }
    let item = unsafe {
        let has = |f: u32| IsClipboardFormatAvailable(f).is_ok();
        if has(CF_UNICODETEXT) {
            GetClipboardData(CF_UNICODETEXT)
                .ok()
                .and_then(|h| bytes(h))
                .and_then(|b| {
                    let u: Vec<u16> = b
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|c| u16::from_le_bytes(*c))
                        .take_while(|&c| c != 0)
                        .collect();
                    let s = String::from_utf16_lossy(&u).replace("\r\n", "\n");
                    (!s.is_empty()).then(|| ClipItem {
                        kind: CLIP_TEXT,
                        data: s.into_bytes(),
                    })
                })
        } else if png_format != 0 && has(png_format) {
            GetClipboardData(png_format)
                .ok()
                .and_then(|h| bytes(h))
                .map(|data| ClipItem {
                    kind: CLIP_PNG,
                    data,
                })
        } else if has(CF_DIB) {
            // Encoded after the clipboard is closed.
            GetClipboardData(CF_DIB)
                .ok()
                .and_then(|h| bytes(h))
                .map(|data| ClipItem { kind: 0, data })
        } else {
            None
        }
    };
    unsafe {
        let _ = CloseClipboard();
    }
    match item {
        Some(ClipItem { kind: 0, data }) => dib_to_png(&data).map(|data| ClipItem {
            kind: CLIP_PNG,
            data,
        }),
        other => other.filter(|i| i.data.len() <= CLIP_MAX),
    }
}

/// Moves `data` into a new global block for `SetClipboardData`.
unsafe fn global(data: &[u8]) -> Option<HGLOBAL> {
    let g = GlobalAlloc(GMEM_MOVEABLE, data.len()).ok()?;
    let p = GlobalLock(g) as *mut u8;
    if p.is_null() {
        let _ = GlobalFree(Some(g));
        return None;
    }
    std::ptr::copy_nonoverlapping(data.as_ptr(), p, data.len());
    let _ = GlobalUnlock(g);
    Some(g)
}

/// Puts an item on the clipboard. False if it couldn't.
fn set(item: &ClipItem, png_format: u32) -> bool {
    // Prepared before opening the clipboard (decoding takes a moment).
    let formats: Vec<(u32, Vec<u8>)> = match item.kind {
        CLIP_TEXT => {
            let s = String::from_utf8_lossy(&item.data)
                .replace("\r\n", "\n")
                .replace('\n', "\r\n");
            let mut b: Vec<u8> = s
                .encode_utf16()
                .chain([0])
                .flat_map(u16::to_le_bytes)
                .collect();
            b.shrink_to_fit();
            vec![(CF_UNICODETEXT, b)]
        }
        CLIP_PNG => {
            let mut v = Vec::new();
            if let Some(dib) = png_to_dib(&item.data) {
                v.push((CF_DIB, dib));
            }
            if png_format != 0 {
                v.push((png_format, item.data.clone()));
            }
            v
        }
        _ => Vec::new(),
    };
    if formats.is_empty() || !open() {
        return false;
    }
    let mut ok = false;
    unsafe {
        if EmptyClipboard().is_ok() {
            for (f, data) in &formats {
                if let Some(g) = global(data) {
                    if SetClipboardData(*f, Some(HANDLE(g.0))).is_ok() {
                        ok = true; // the clipboard owns the block now
                    } else {
                        let _ = GlobalFree(Some(g));
                    }
                }
            }
        }
        let _ = CloseClipboard();
    }
    ok
}

/// A 24- or 32-bit DIB (BITMAPINFOHEADER and pixels) as a PNG.
fn dib_to_png(d: &[u8]) -> Option<Vec<u8>> {
    let u32_at = |i: usize| u32::from_le_bytes(d[i..i + 4].try_into().unwrap());
    if d.len() < 40 {
        return None;
    }
    let header = u32_at(0) as usize;
    let w = u32_at(4) as i32;
    let h = u32_at(8) as i32;
    let bpp = u16::from_le_bytes([d[14], d[15]]) as usize;
    let compression = u32_at(16);
    let palette = u32_at(32) as usize;
    if w <= 0 || h == 0 || !(bpp == 24 || bpp == 32) || !(compression == 0 || compression == 3) {
        return None;
    }
    let (w, rows, top_down) = (w as usize, h.unsigned_abs() as usize, h < 0);
    if w * rows > MAX_PIXELS {
        return None;
    }
    // BI_BITFIELDS with a plain header: three masks follow it (assumed the
    // usual BGRA order, which is what Windows produces).
    let masks = if compression == 3 && header == 40 {
        12
    } else {
        0
    };
    let start = header + masks + palette * 4;
    let px = bpp / 8;
    let stride = (w * px + 3) & !3;
    if d.len() < start + stride * rows {
        return None;
    }
    let mut rgba = Vec::with_capacity(w * rows * 4);
    for y in 0..rows {
        let row = if top_down { y } else { rows - 1 - y };
        let line = &d[start + row * stride..][..w * px];
        for p in line.chunks_exact(px) {
            rgba.extend_from_slice(&[p[2], p[1], p[0], if px == 4 { p[3] } else { 255 }]);
        }
    }
    // Most 32-bit DIBs leave alpha at 0 meaning "opaque".
    if px == 4 && rgba.as_chunks::<4>().0.iter().all(|p| p[3] == 0) {
        rgba.as_chunks_mut::<4>()
            .0
            .iter_mut()
            .for_each(|p| p[3] = 255);
    }
    let mut out = Vec::new();
    {
        let mut e = png::Encoder::new(&mut out, w as u32, rows as u32);
        e.set_color(png::ColorType::Rgba);
        e.set_depth(png::BitDepth::Eight);
        e.set_compression(png::Compression::Fast);
        let mut wr = e.write_header().ok()?;
        wr.write_image_data(&rgba).ok()?;
    }
    (out.len() <= CLIP_MAX).then_some(out)
}

/// A PNG as a 32-bit bottom-up DIB (what most apps paste).
fn png_to_dib(data: &[u8]) -> Option<Vec<u8>> {
    let mut dec = png::Decoder::new(std::io::Cursor::new(data));
    dec.set_transformations(png::Transformations::normalize_to_color8());
    let mut r = dec.read_info().ok()?;
    let (w, h) = {
        let i = r.info();
        (i.width as usize, i.height as usize)
    };
    if w == 0 || h == 0 || w * h > MAX_PIXELS {
        return None;
    }
    let mut buf = vec![0; r.output_buffer_size()?];
    let info = r.next_frame(&mut buf).ok()?;
    let channels = match info.color_type {
        png::ColorType::Grayscale => 1,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        png::ColorType::Indexed => return None, // expanded by the transformations
    };
    let mut dib = Vec::with_capacity(40 + w * h * 4);
    let le32 = |v: u32| v.to_le_bytes();
    dib.extend_from_slice(&le32(40));
    dib.extend_from_slice(&le32(w as u32));
    dib.extend_from_slice(&le32(h as u32)); // positive: bottom-up
    dib.extend_from_slice(&1u16.to_le_bytes());
    dib.extend_from_slice(&32u16.to_le_bytes());
    dib.extend_from_slice(&le32(0)); // BI_RGB
    dib.extend_from_slice(&le32((w * h * 4) as u32));
    dib.extend_from_slice(&[0; 16]); // resolution, palette
    for y in (0..h).rev() {
        let line = &buf[y * info.line_size..][..w * channels];
        for p in line.chunks_exact(channels) {
            let (r, g, b, a) = match channels {
                1 => (p[0], p[0], p[0], 255),
                2 => (p[0], p[0], p[0], p[1]),
                3 => (p[0], p[1], p[2], 255),
                _ => (p[0], p[1], p[2], p[3]),
            };
            dib.extend_from_slice(&[b, g, r, a]);
        }
    }
    Some(dib)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn images_survive_dib_and_png() {
        // 3 x 2, bottom-up 24-bit DIB with row padding.
        let (w, h) = (3usize, 2usize);
        let stride = (w * 3 + 3) & !3;
        let mut d = vec![0u8; 40];
        d[0..4].copy_from_slice(&40u32.to_le_bytes());
        d[4..8].copy_from_slice(&(w as u32).to_le_bytes());
        d[8..12].copy_from_slice(&(h as u32).to_le_bytes());
        d[12..14].copy_from_slice(&1u16.to_le_bytes());
        d[14..16].copy_from_slice(&24u16.to_le_bytes());
        for y in 0..h {
            let mut line = vec![0u8; stride];
            for x in 0..w {
                line[x * 3..x * 3 + 3].copy_from_slice(&[x as u8, y as u8, 200]);
                // BGR
            }
            d.extend_from_slice(&line);
        }
        let png = dib_to_png(&d).expect("png");
        let dib = png_to_dib(&png).expect("dib");
        // Back as 32-bit bottom-up: same pixels, opaque.
        for y in 0..h {
            for x in 0..w {
                let i = 40 + (y * w + x) * 4;
                assert_eq!(&dib[i..i + 4], &[x as u8, y as u8, 200, 255]);
            }
        }
    }
}

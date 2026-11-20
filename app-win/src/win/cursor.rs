//! Watches the PC's mouse pointer (shape + shown/hidden) so the viewer can draw
//! it locally. A shape is read only when the cursor handle changes.
//!
//! The image is rendered with `DrawIconEx` onto black and onto white; the two
//! results give straight alpha (`a = 255 - (white - black)`) and colour for any
//! cursor type (colour, alpha, monochrome). Inverting cursors (e.g. the I-beam
//! on some themes) come out as black/white.

use super::Res;
use glass_core::protocol::CURSOR_MAX_DIM;
use windows::Win32::Foundation::POINT;
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetObjectW, SelectObject, BITMAP,
    BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP, HGDIOBJ,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DrawIconEx, GetCursorInfo, GetIconInfo, CURSORINFO, CURSOR_SHOWING, DI_NORMAL, HCURSOR, HICON,
    ICONINFO,
};

/// A pointer image ready for `CursorShape`.
pub struct Shape {
    pub width: u16,
    pub height: u16,
    pub hot_x: u16,
    pub hot_y: u16,
    pub rgba: Vec<u8>,
}

/// What changed since the last poll.
pub enum Change {
    None,
    Visible(bool),
    Shape(Shape),
}

pub struct CursorWatch {
    last: Option<HCURSOR>,
    visible: Option<bool>,
}

impl CursorWatch {
    pub fn new() -> Self {
        Self {
            last: None,
            visible: None,
        }
    }

    /// Cheap when nothing changed (one `GetCursorInfo`).
    pub fn poll(&mut self) -> Change {
        let mut ci = CURSORINFO {
            cbSize: std::mem::size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };
        if unsafe { GetCursorInfo(&mut ci) }.is_err() {
            return Change::None;
        }
        let showing = ci.flags.0 & CURSOR_SHOWING.0 != 0 && !ci.hCursor.is_invalid();
        if self.visible != Some(showing) {
            self.visible = Some(showing);
            if !showing {
                return Change::Visible(false);
            }
            if self.last == Some(ci.hCursor) {
                return Change::Visible(true);
            }
        }
        if !showing || self.last == Some(ci.hCursor) {
            return Change::None;
        }
        match read_shape(ci.hCursor) {
            Ok(s) => {
                self.last = Some(ci.hCursor);
                Change::Shape(s)
            }
            Err(_) => Change::None,
        }
    }
}

fn bitmap_size(h: HBITMAP) -> Option<(i32, i32)> {
    if h.is_invalid() {
        return None;
    }
    let mut bm = BITMAP::default();
    let n = unsafe {
        GetObjectW(
            HGDIOBJ(h.0),
            std::mem::size_of::<BITMAP>() as i32,
            Some(&mut bm as *mut _ as *mut _),
        )
    };
    (n != 0).then_some((bm.bmWidth, bm.bmHeight))
}

fn read_shape(cur: HCURSOR) -> Res<Shape> {
    let mut ii = ICONINFO::default();
    unsafe { GetIconInfo(HICON(cur.0), &mut ii)? };
    // Monochrome cursors have no colour bitmap and a double-height mask.
    let (w, h) = match bitmap_size(ii.hbmColor) {
        Some(sz) => sz,
        None => {
            let (w, h2) = bitmap_size(ii.hbmMask).ok_or("cursor has no bitmaps")?;
            (w, h2 / 2)
        }
    };
    unsafe {
        if !ii.hbmColor.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(ii.hbmColor.0));
        }
        if !ii.hbmMask.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(ii.hbmMask.0));
        }
    }
    if w <= 0 || h <= 0 || w > 256 || h > 256 {
        return Err("odd cursor size".into());
    }
    let black = render(cur, w, h, 0x00)?;
    let white = render(cur, w, h, 0xFF)?;
    let mut rgba = vec![0u8; (w * h * 4) as usize];
    for i in 0..(w * h) as usize {
        let (b, wt) = (&black[i * 4..i * 4 + 4], &white[i * 4..i * 4 + 4]);
        // BGRA in the DIB. alpha from how much the background shows through.
        let diff = (0..3)
            .map(|c| wt[c] as i32 - b[c] as i32)
            .max()
            .unwrap_or(0);
        let a = (255 - diff).clamp(0, 255);
        let o = &mut rgba[i * 4..i * 4 + 4];
        if a > 0 {
            // On black, colour = alpha * c  ->  c = black / alpha.
            for (dst, src) in [(0usize, 2usize), (1, 1), (2, 0)] {
                o[dst] = ((b[src] as i32 * 255 + a / 2) / a).min(255) as u8;
            }
        }
        o[3] = a as u8;
    }
    let (hot_x, hot_y) = (ii.xHotspot as i32, ii.yHotspot as i32);
    Ok(fit(Shape {
        width: w as u16,
        height: h as u16,
        hot_x: hot_x.clamp(0, w - 1) as u16,
        hot_y: hot_y.clamp(0, h - 1) as u16,
        rgba,
    }))
}

/// Draws the cursor onto a solid background; returns BGRA rows, top-down.
fn render(cur: HCURSOR, w: i32, h: i32, bg: u8) -> Res<Vec<u8>> {
    unsafe {
        let dc = CreateCompatibleDC(None);
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let dib = CreateDIBSection(Some(dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0)?;
        let old = SelectObject(dc, HGDIOBJ(dib.0));
        let len = (w * h * 4) as usize;
        std::ptr::write_bytes(bits as *mut u8, bg, len);
        let ok = DrawIconEx(dc, 0, 0, HICON(cur.0), w, h, 0, None, DI_NORMAL);
        let out = std::slice::from_raw_parts(bits as *const u8, len).to_vec();
        SelectObject(dc, old);
        let _ = DeleteObject(HGDIOBJ(dib.0));
        let _ = DeleteDC(dc);
        ok?;
        Ok(out)
    }
}

/// Scales a cursor down (nearest neighbour) to fit `CURSOR_MAX_DIM`.
fn fit(s: Shape) -> Shape {
    let max = CURSOR_MAX_DIM;
    if s.width <= max && s.height <= max {
        return s;
    }
    let k = (s.width.max(s.height) as f32) / max as f32;
    let (w, h) = (
        ((s.width as f32 / k) as u16).max(1),
        ((s.height as f32 / k) as u16).max(1),
    );
    let mut rgba = vec![0u8; w as usize * h as usize * 4];
    for y in 0..h as usize {
        for x in 0..w as usize {
            let sx = ((x as f32 * k) as usize).min(s.width as usize - 1);
            let sy = ((y as f32 * k) as usize).min(s.height as usize - 1);
            let si = (sy * s.width as usize + sx) * 4;
            let di = (y * w as usize + x) * 4;
            rgba[di..di + 4].copy_from_slice(&s.rgba[si..si + 4]);
        }
    }
    Shape {
        width: w,
        height: h,
        hot_x: ((s.hot_x as f32 / k) as u16).min(w - 1),
        hot_y: ((s.hot_y as f32 / k) as u16).min(h - 1),
        rgba,
    }
}

/// Current pointer position in screen pixels (unused by M3's viewer, which
/// draws its own pointer where the user's mouse is). LATER: follow host-side moves.
#[allow(dead_code)]
pub fn position() -> Option<(i32, i32)> {
    let mut p = POINT::default();
    unsafe { windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut p).ok()? };
    Some((p.x, p.y))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_scales_big_cursors() {
        let s = Shape {
            width: 192,
            height: 96,
            hot_x: 100,
            hot_y: 50,
            rgba: vec![7; 192 * 96 * 4],
        };
        let f = fit(s);
        assert_eq!((f.width, f.height), (96, 48));
        assert_eq!((f.hot_x, f.hot_y), (50, 25));
        assert_eq!(f.rgba.len(), 96 * 48 * 4);
    }

    #[test]
    fn reads_the_arrow() {
        use windows::Win32::UI::WindowsAndMessaging::{LoadCursorW, IDC_ARROW};
        let arrow = unsafe { LoadCursorW(None, IDC_ARROW) }.unwrap();
        let s = read_shape(arrow).unwrap();
        assert!(s.width >= 16 && s.height >= 16);
        // Mostly transparent, with an opaque outline near the hotspot.
        let opaque = s.rgba.chunks(4).filter(|p| p[3] > 200).count();
        let clear = s.rgba.chunks(4).filter(|p| p[3] == 0).count();
        assert!(
            opaque > 20 && clear > opaque,
            "opaque {opaque} clear {clear}"
        );
        assert!(s.hot_x < 4 && s.hot_y < 4);
    }
}

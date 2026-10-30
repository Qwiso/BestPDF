//! Small GDI helpers: solid fills, translucent fills, text, fonts, page blits.

use windows::Win32::Foundation::{COLORREF, RECT, SIZE};
use windows::Win32::Graphics::Gdi::*;

use crate::document::PageBitmap;

pub fn rect(x: i32, y: i32, w: i32, h: i32) -> RECT {
    RECT { left: x, top: y, right: x + w, bottom: y + h }
}

pub fn contains(r: &RECT, x: i32, y: i32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

pub fn fill(dc: HDC, r: &RECT, color: COLORREF) {
    unsafe {
        SetDCBrushColor(dc, color);
        FillRect(dc, r, HBRUSH(GetStockObject(DC_BRUSH).0));
    }
}

pub fn round_fill(dc: HDC, r: &RECT, radius: i32, color: COLORREF) {
    unsafe {
        let old_pen = SelectObject(dc, GetStockObject(DC_PEN));
        let old_brush = SelectObject(dc, GetStockObject(DC_BRUSH));
        SetDCPenColor(dc, color);
        SetDCBrushColor(dc, color);
        let _ = RoundRect(dc, r.left, r.top, r.right, r.bottom, radius, radius);
        SelectObject(dc, old_pen);
        SelectObject(dc, old_brush);
    }
}

/// A 1×1 source DC used to draw translucent rectangles with AlphaBlend.
pub struct Tint {
    dc: HDC,
}

impl Tint {
    pub fn new() -> Self {
        unsafe {
            let dc = CreateCompatibleDC(None);
            let screen = GetDC(None);
            SelectObject(dc, CreateCompatibleBitmap(screen, 1, 1).into());
            ReleaseDC(None, screen);
            Self { dc }
        }
    }

    pub fn fill(&self, dest: HDC, r: &RECT, color: COLORREF, alpha: u8) {
        unsafe {
            SetPixel(self.dc, 0, 0, color);
            let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: alpha, AlphaFormat: 0 };
            let _ = AlphaBlend(dest, r.left, r.top, r.right - r.left, r.bottom - r.top, self.dc, 0, 0, 1, 1, blend);
        }
    }
}

pub fn font(face: &str, logical_size: i32, dpi: u32, weight: i32) -> HFONT {
    let mut lf = LOGFONTW {
        lfHeight: -(logical_size * dpi as i32 / 96),
        lfWeight: weight,
        lfQuality: CLEARTYPE_QUALITY,
        ..Default::default()
    };
    for (dst, src) in lf.lfFaceName.iter_mut().zip(face.encode_utf16().take(31)) {
        *dst = src;
    }
    unsafe { CreateFontIndirectW(&lf) }
}

pub fn delete_font(f: HFONT) {
    unsafe { let _ = DeleteObject(f.into()); }
}

pub fn text(dc: HDC, font: HFONT, color: COLORREF, s: &str, mut r: RECT, flags: DRAW_TEXT_FORMAT) {
    let mut wide: Vec<u16> = s.encode_utf16().collect();
    unsafe {
        let old = SelectObject(dc, font.into());
        SetBkMode(dc, TRANSPARENT);
        SetTextColor(dc, color);
        DrawTextW(dc, &mut wide, &mut r, flags | DT_SINGLELINE | DT_NOPREFIX);
        SelectObject(dc, old);
    }
}

pub fn text_width(dc: HDC, font: HFONT, s: &str) -> i32 {
    let wide: Vec<u16> = s.encode_utf16().collect();
    let mut size = SIZE::default();
    unsafe {
        let old = SelectObject(dc, font.into());
        let _ = GetTextExtentPoint32W(dc, &wide, &mut size);
        SelectObject(dc, old);
    }
    size.cx
}

/// Draws a page bitmap; 1:1 when sizes match, smooth stretch otherwise.
pub fn blit(dc: HDC, bmp: &PageBitmap, x: i32, y: i32, w: i32, h: i32) {
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: bmp.width,
            biHeight: -bmp.height, // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let exact = bmp.width == w && bmp.height == h;
    unsafe {
        SetStretchBltMode(dc, if exact { COLORONCOLOR } else { HALFTONE });
        StretchDIBits(
            dc, x, y, w, h, 0, 0, bmp.width, bmp.height,
            Some(bmp.pixels.as_ptr().cast()), &info, DIB_RGB_COLORS, SRCCOPY,
        );
    }
}

/// Off-screen buffer the whole client area is painted into, then blitted once.
pub struct BackBuffer {
    pub dc: HDC,
    bitmap: HBITMAP,
    w: i32,
    h: i32,
}

impl BackBuffer {
    pub fn ensure(slot: &mut Option<BackBuffer>, screen: HDC, w: i32, h: i32) -> HDC {
        if let Some(b) = slot {
            if b.w == w && b.h == h {
                return b.dc;
            }
        }
        *slot = None; // drop the old one first
        unsafe {
            let dc = CreateCompatibleDC(Some(screen));
            let bitmap = CreateCompatibleBitmap(screen, w, h);
            SelectObject(dc, bitmap.into());
            *slot = Some(BackBuffer { dc, bitmap, w, h });
        }
        slot.as_ref().unwrap().dc
    }
}

impl Drop for BackBuffer {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteDC(self.dc);
            let _ = DeleteObject(self.bitmap.into());
        }
    }
}

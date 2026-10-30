//! Things placed on top of pages: text, highlights and signatures.
//!
//! Positions are PDF points with the origin at the page's top-left corner as
//! displayed (after any /Rotate). Overlays are drawn by us while editing;
//! stamp.rs writes them into the PDF on save and reads them back on open.

use std::cell::RefCell;
use std::rc::Rc;

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Gdi::*;

use crate::gdi;

/// Text layout as fractions of the font size. Arial on screen and Helvetica in
/// the PDF share these metrics, so boxes match what gets saved.
pub const ASCENT: f32 = 0.905;
pub const LINE: f32 = 1.15;
/// Highlighter yellow. Saved with the Multiply blend mode; on screen, ANDing
/// with pure yellow is exactly that multiply.
pub const HIGHLIGHT: (u8, u8, u8) = (255, 255, 0);
pub const TEXT_FACE: &str = "Arial";
/// Smallest box side, in points, a resize can produce.
pub const MIN_SIDE: f32 = 4.0;

#[derive(Clone)]
pub enum Kind {
    Text { text: String, size: f32 },
    Highlight,
    Image(Rc<Image>),
}

#[derive(Clone)]
pub struct Overlay {
    pub page: usize,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub kind: Kind,
}

impl Overlay {
    pub fn text(page: usize, x: f32, y: f32, text: String, size: f32) -> Self {
        let mut o = Self { page, x, y, w: 0.0, h: 0.0, kind: Kind::Text { text, size } };
        o.fit_text();
        o
    }

    /// Recomputes a text box's size from its text.
    pub fn fit_text(&mut self) {
        if let Kind::Text { text, size } = &self.kind {
            (self.w, self.h) = text_size(text, *size);
        }
    }

    /// Text and signatures scale as a whole; highlights stretch freely.
    pub fn keeps_aspect(&self) -> bool {
        !matches!(self.kind, Kind::Highlight)
    }

    pub fn text_mut(&mut self) -> Option<&mut String> {
        match &mut self.kind {
            Kind::Text { text, .. } => Some(text),
            _ => None,
        }
    }

    /// Draws the overlay for a page whose top-left is at (ox, oy) on `dc`,
    /// at `s` pixels per point. `alpha` fades images (stamp preview).
    pub fn paint(&self, dc: HDC, ox: f32, oy: f32, s: f32, alpha: u8) {
        let r = self.screen_rect(ox, oy, s);
        match &self.kind {
            Kind::Highlight => unsafe {
                let old = SelectObject(dc, GetStockObject(DC_BRUSH));
                let (cr, cg, cb) = HIGHLIGHT;
                SetDCBrushColor(dc, crate::theme::rgb((cr as u32) << 16 | (cg as u32) << 8 | cb as u32));
                // DPa: destination AND pattern.
                let _ = PatBlt(dc, r.left, r.top, (r.right - r.left).max(1), (r.bottom - r.top).max(1), ROP_CODE(0x00A0_00C9));
                SelectObject(dc, old);
            },
            Kind::Text { text, size } => {
                let px = (size * s).round() as i32;
                if px < 1 {
                    return;
                }
                let font = gdi::font_px(TEXT_FACE, px, 400);
                for (i, line) in text.split('\n').enumerate() {
                    let top = r.top + (i as f32 * LINE * size * s).round() as i32;
                    let line_rect = RECT { left: r.left, top, right: r.left + 100_000, bottom: top + px * 2 };
                    gdi::text(dc, font, crate::theme::rgb(0x000000), line, line_rect, DT_LEFT | DT_TOP | DT_NOCLIP);
                }
                gdi::delete_font(font);
            }
            Kind::Image(img) => img.draw(dc, &r, alpha),
        }
    }

    pub fn screen_rect(&self, ox: f32, oy: f32, s: f32) -> RECT {
        let left = (ox + self.x * s).round() as i32;
        let top = (oy + self.y * s).round() as i32;
        RECT { left, top, right: (ox + (self.x + self.w) * s).round() as i32, bottom: (oy + (self.y + self.h) * s).round() as i32 }
    }
}

// ---- text measurement ------------------------------------------------------

thread_local! {
    /// A DC with Arial at a large em size, so widths keep sub-point precision.
    static MEASURE: (HDC, HFONT) = unsafe {
        let dc = CreateCompatibleDC(None);
        let font = gdi::font_px(TEXT_FACE, MEASURE_EM as i32, 400);
        SelectObject(dc, font.into());
        (dc, font)
    };
}
const MEASURE_EM: f32 = 1000.0;

/// Width in points of one line of text at `size` points.
pub fn text_width(line: &str, size: f32) -> f32 {
    if line.is_empty() {
        return 0.0;
    }
    MEASURE.with(|&(dc, font)| gdi::text_width(dc, font, line) as f32 * size / MEASURE_EM)
}

/// Box (w, h) in points for multi-line text. Empty text keeps a sliver of
/// width so the box stays visible while typing.
pub fn text_size(text: &str, size: f32) -> (f32, f32) {
    let lines = text.split('\n');
    let w = lines.clone().map(|l| text_width(l, size)).fold(size * 0.4, f32::max);
    (w, lines.count() as f32 * LINE * size)
}

// ---- images ----------------------------------------------------------------

/// A BGRA image with straight (non-premultiplied) alpha, as PDFium and PNG use.
pub struct Image {
    pub w: i32,
    pub h: i32,
    pub bgra: Vec<u8>,
    /// Recently drawn sizes, premultiplied, ready for AlphaBlend.
    scaled: RefCell<Vec<Scaled>>,
}

struct Scaled {
    w: i32,
    h: i32,
    dc: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
}

impl Drop for Scaled {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old);
            let _ = DeleteDC(self.dc);
            let _ = DeleteObject(self.bitmap.into());
        }
    }
}

impl Image {
    pub fn new(w: i32, h: i32, bgra: Vec<u8>) -> Rc<Self> {
        debug_assert_eq!(bgra.len(), (w * h * 4) as usize);
        Rc::new(Self { w, h, bgra, scaled: RefCell::new(Vec::new()) })
    }

    /// Resamples to w×h, premultiplied BGRA: box filter when shrinking,
    /// bilinear when enlarging (zoomed in past the image's resolution).
    pub fn resample(&self, w: i32, h: i32) -> Vec<u8> {
        let (sw, sh) = (self.w as usize, self.h as usize);
        let (w, h) = (w.max(1) as usize, h.max(1) as usize);
        if w > sw || h > sh {
            return self.bilinear(w, h);
        }
        let mut out = vec![0u8; w * h * 4];
        for ty in 0..h {
            let y0 = ty * sh / h;
            let y1 = ((ty + 1) * sh / h).max(y0 + 1).min(sh);
            for tx in 0..w {
                let x0 = tx * sw / w;
                let x1 = ((tx + 1) * sw / w).max(x0 + 1).min(sw);
                let mut acc = [0u32; 4];
                for y in y0..y1 {
                    for x in x0..x1 {
                        let p = &self.bgra[(y * sw + x) * 4..][..4];
                        let a = p[3] as u32;
                        acc[0] += p[0] as u32 * a / 255;
                        acc[1] += p[1] as u32 * a / 255;
                        acc[2] += p[2] as u32 * a / 255;
                        acc[3] += a;
                    }
                }
                let n = ((y1 - y0) * (x1 - x0)) as u32;
                let o = &mut out[(ty * w + tx) * 4..][..4];
                for c in 0..4 {
                    o[c] = (acc[c] / n) as u8;
                }
            }
        }
        out
    }

    fn bilinear(&self, w: usize, h: usize) -> Vec<u8> {
        let (sw, sh) = (self.w as usize, self.h as usize);
        let premul = |x: usize, y: usize| {
            let p = &self.bgra[(y * sw + x) * 4..][..4];
            let a = p[3] as f32 / 255.0;
            [p[0] as f32 * a, p[1] as f32 * a, p[2] as f32 * a, p[3] as f32]
        };
        let mut out = vec![0u8; w * h * 4];
        for ty in 0..h {
            let fy = ((ty as f32 + 0.5) * sh as f32 / h as f32 - 0.5).clamp(0.0, (sh - 1) as f32);
            let (y0, ky) = (fy as usize, fy.fract());
            let y1 = (y0 + 1).min(sh - 1);
            for tx in 0..w {
                let fx = ((tx as f32 + 0.5) * sw as f32 / w as f32 - 0.5).clamp(0.0, (sw - 1) as f32);
                let (x0, kx) = (fx as usize, fx.fract());
                let x1 = (x0 + 1).min(sw - 1);
                let (a, b, c, d) = (premul(x0, y0), premul(x1, y0), premul(x0, y1), premul(x1, y1));
                let o = &mut out[(ty * w + tx) * 4..][..4];
                for i in 0..4 {
                    let top = a[i] + (b[i] - a[i]) * kx;
                    let bottom = c[i] + (d[i] - c[i]) * kx;
                    o[i] = (top + (bottom - top) * ky).round() as u8;
                }
            }
        }
        out
    }

    /// Resampled copy with straight alpha (for shrinking imported images).
    pub fn resized(&self, w: i32, h: i32) -> Rc<Image> {
        let mut px = self.resample(w, h);
        for p in px.chunks_exact_mut(4) {
            let a = p[3] as u32;
            if a > 0 {
                for c in 0..3 {
                    p[c] = (p[c] as u32 * 255 / a).min(255) as u8;
                }
            }
        }
        Image::new(w.max(1), h.max(1), px)
    }

    pub fn draw(&self, dc: HDC, r: &RECT, alpha: u8) {
        let (w, h) = (r.right - r.left, r.bottom - r.top);
        if w <= 0 || h <= 0 || w > 8000 || h > 8000 {
            return;
        }
        let mut cache = self.scaled.borrow_mut();
        let i = match cache.iter().position(|s| s.w == w && s.h == h) {
            Some(i) => i,
            None => {
                if cache.len() >= 4 {
                    cache.remove(0);
                }
                let Some(s) = self.make_scaled(w, h) else { return };
                cache.push(s);
                cache.len() - 1
            }
        };
        let s = &cache[i];
        let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: alpha, AlphaFormat: AC_SRC_ALPHA as u8 };
        unsafe {
            let _ = AlphaBlend(dc, r.left, r.top, w, h, s.dc, 0, 0, w, h, blend);
        }
    }

    fn make_scaled(&self, w: i32, h: i32) -> Option<Scaled> {
        let px = self.resample(w, h);
        unsafe {
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits = std::ptr::null_mut();
            let bitmap = CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
            std::ptr::copy_nonoverlapping(px.as_ptr(), bits as *mut u8, px.len());
            let dc = CreateCompatibleDC(None);
            let old = SelectObject(dc, bitmap.into());
            Some(Scaled { w, h, dc, bitmap, old })
        }
    }
}

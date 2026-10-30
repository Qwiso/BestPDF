//! Things placed on top of pages: text, highlights and signatures.
//!
//! Positions are PDF points with the origin at the page's top-left corner as
//! displayed (after any /Rotate). Overlays are drawn by us while editing;
//! stamp.rs writes them into the PDF on save and reads them back on open.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Gdi::*;

use crate::gdi;

/// Line height as a fraction of the font size.
pub const LINE: f32 = 1.15;
/// Smallest box side, in points, a resize can produce.
pub const MIN_SIDE: f32 = 4.0;

/// Text colors offered in the options strip (0xRRGGBB).
pub const TEXT_COLORS: [u32; 5] = [0x000000, 0x1F4FD1, 0xD32F2F, 0x2E7D32, 0x757575];
/// Marker colors: yellow, green, blue, pink, orange, red.
pub const MARK_COLORS: [u32; 6] = [0xFFEB3B, 0x8BE78B, 0x80D4FF, 0xFF9ECF, 0xFFB74D, 0xE53935];
/// Font sizes the − / + buttons step through, in points.
pub const TEXT_SIZES: [f32; 17] = [6.0, 8.0, 9.0, 10.0, 11.0, 12.0, 14.0, 16.0, 18.0, 20.0, 24.0, 28.0, 32.0, 36.0, 48.0, 64.0, 72.0];

/// Font families. Each is one of the PDF standard fonts (nothing to embed)
/// with a metric-compatible Windows font for the screen.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum Family {
    Sans,
    Serif,
    Mono,
}

impl Family {
    pub const ALL: [Family; 3] = [Family::Sans, Family::Serif, Family::Mono];

    pub fn label(self) -> &'static str {
        match self {
            Family::Sans => "Sans",
            Family::Serif => "Serif",
            Family::Mono => "Mono",
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Family::Sans => "sans",
            Family::Serif => "serif",
            Family::Mono => "mono",
        }
    }

    pub fn from_key(key: &str) -> Family {
        Family::ALL.into_iter().find(|f| f.key() == key).unwrap_or(Family::Sans)
    }

    fn screen_face(self) -> &'static str {
        match self {
            Family::Sans => "Arial",
            Family::Serif => "Times New Roman",
            Family::Mono => "Courier New",
        }
    }

    /// The standard-14 PDF font name.
    pub fn pdf_name(self, bold: bool, italic: bool) -> &'static std::ffi::CStr {
        match (self, bold, italic) {
            (Family::Sans, false, false) => c"Helvetica",
            (Family::Sans, true, false) => c"Helvetica-Bold",
            (Family::Sans, false, true) => c"Helvetica-Oblique",
            (Family::Sans, true, true) => c"Helvetica-BoldOblique",
            (Family::Serif, false, false) => c"Times-Roman",
            (Family::Serif, true, false) => c"Times-Bold",
            (Family::Serif, false, true) => c"Times-Italic",
            (Family::Serif, true, true) => c"Times-BoldItalic",
            (Family::Mono, false, false) => c"Courier",
            (Family::Mono, true, false) => c"Courier-Bold",
            (Family::Mono, false, true) => c"Courier-Oblique",
            (Family::Mono, true, true) => c"Courier-BoldOblique",
        }
    }

    /// The Windows font file to embed when text needs characters the
    /// standard fonts lack.
    pub fn ttf(self, bold: bool, italic: bool) -> &'static str {
        let names = match self {
            Family::Sans => ["arial.ttf", "arialbd.ttf", "ariali.ttf", "arialbi.ttf"],
            Family::Serif => ["times.ttf", "timesbd.ttf", "timesi.ttf", "timesbi.ttf"],
            Family::Mono => ["cour.ttf", "courbd.ttf", "couri.ttf", "courbi.ttf"],
        };
        names[bold as usize + 2 * italic as usize]
    }
}

#[derive(Clone, Copy, PartialEq)]
pub struct TextStyle {
    pub family: Family,
    pub size: f32,
    pub bold: bool,
    pub italic: bool,
    /// 0xRRGGBB
    pub color: u32,
}

impl Default for TextStyle {
    fn default() -> Self {
        Self { family: Family::Sans, size: 14.0, bold: false, italic: false, color: 0x000000 }
    }
}

/// How a marked area is drawn.
#[derive(Clone, Copy, PartialEq)]
pub enum Mark {
    /// Filled, multiplied over the page like a highlighter pen.
    Highlight,
    Underline,
    Strike,
}

impl Mark {
    pub const ALL: [Mark; 3] = [Mark::Highlight, Mark::Underline, Mark::Strike];

    pub fn key(self) -> &'static str {
        match self {
            Mark::Highlight => "highlight",
            Mark::Underline => "underline",
            Mark::Strike => "strike",
        }
    }

    pub fn from_key(key: &str) -> Mark {
        Mark::ALL.into_iter().find(|m| m.key() == key).unwrap_or(Mark::Highlight)
    }
}

#[derive(Clone, Copy, PartialEq)]
pub struct MarkStyle {
    pub mark: Mark,
    /// 0xRRGGBB
    pub color: u32,
}

impl Default for MarkStyle {
    fn default() -> Self {
        Self { mark: Mark::Highlight, color: MARK_COLORS[0] }
    }
}

#[derive(Clone)]
pub enum Kind {
    Text { text: String, style: TextStyle },
    Mark(MarkStyle),
    Image(Rc<Image>),
}

impl PartialEq for Kind {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Kind::Text { text: a, style: s }, Kind::Text { text: b, style: t }) => a == b && s == t,
            (Kind::Mark(a), Kind::Mark(b)) => a == b,
            (Kind::Image(a), Kind::Image(b)) => Rc::ptr_eq(a, b),
            _ => false,
        }
    }
}

#[derive(Clone, PartialEq)]
pub struct Overlay {
    pub page: usize,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub kind: Kind,
}

impl Overlay {
    pub fn text(page: usize, x: f32, y: f32, text: String, style: TextStyle) -> Self {
        let mut o = Self { page, x, y, w: 0.0, h: 0.0, kind: Kind::Text { text, style } };
        o.fit_text();
        o
    }

    /// Recomputes a text box's size from its text.
    pub fn fit_text(&mut self) {
        if let Kind::Text { text, style } = &self.kind {
            (self.w, self.h) = text_size(text, style);
        }
    }

    /// Text and signatures scale as a whole; marks stretch freely.
    pub fn keeps_aspect(&self) -> bool {
        !matches!(self.kind, Kind::Mark(_))
    }

    pub fn text_mut(&mut self) -> Option<&mut String> {
        match &mut self.kind {
            Kind::Text { text, .. } => Some(text),
            _ => None,
        }
    }

    /// For underline and strikethrough: the line's top and thickness within
    /// the box, in points.
    pub fn mark_bar(&self) -> Option<(f32, f32)> {
        let Kind::Mark(style) = &self.kind else { return None };
        let t = (self.h * 0.08).clamp(0.75, 3.0).min(self.h);
        match style.mark {
            Mark::Highlight => None,
            Mark::Underline => Some((self.h - t, t)),
            Mark::Strike => Some(((self.h - t) / 2.0, t)),
        }
    }

    /// Draws the overlay for a page whose top-left is at (ox, oy) on `dc`,
    /// at `s` pixels per point. `alpha` fades images (stamp preview).
    pub fn paint(&self, dc: HDC, ox: f32, oy: f32, s: f32, alpha: u8) {
        let r = self.screen_rect(ox, oy, s);
        match &self.kind {
            Kind::Mark(style) => match self.mark_bar() {
                None => gdi::multiply(dc, &r, style.color),
                Some((top, t)) => {
                    let y = (oy + (self.y + top) * s).round() as i32;
                    let h = ((t * s).round() as i32).max(1);
                    gdi::fill(dc, &RECT { top: y, bottom: y + h, ..r }, crate::theme::rgb(style.color));
                }
            },
            Kind::Text { text, style } => {
                let px = (style.size * s).round() as i32;
                if px < 1 {
                    return;
                }
                let font = gdi::font_styled(style.family.screen_face(), px, if style.bold { 700 } else { 400 }, style.italic);
                for (i, line) in text.split('\n').enumerate() {
                    let top = r.top + (i as f32 * LINE * style.size * s).round() as i32;
                    let line_rect = RECT { left: r.left, top, right: r.left + 100_000, bottom: top + px * 2 };
                    gdi::text(dc, font, crate::theme::rgb(style.color), line, line_rect, DT_LEFT | DT_TOP | DT_NOCLIP);
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

const MEASURE_EM: f32 = 1000.0;

/// A measuring font at a large em size (so widths keep sub-point precision)
/// and its ascent as a fraction of the em.
struct Measure {
    font: HFONT,
    ascent: f32,
}

thread_local! {
    static MEASURE_DC: HDC = unsafe { CreateCompatibleDC(None) };
    static MEASURE: RefCell<HashMap<(Family, bool, bool), Measure>> = RefCell::new(HashMap::new());
}

fn with_measure<R>(style: &TextStyle, f: impl FnOnce(HDC, &Measure) -> R) -> R {
    MEASURE_DC.with(|&dc| {
        MEASURE.with(|cache| {
            let mut cache = cache.borrow_mut();
            let m = cache.entry((style.family, style.bold, style.italic)).or_insert_with(|| {
                let weight = if style.bold { 700 } else { 400 };
                let font = gdi::font_styled(style.family.screen_face(), MEASURE_EM as i32, weight, style.italic);
                let mut tm = TEXTMETRICW::default();
                unsafe {
                    let old = SelectObject(dc, font.into());
                    let _ = GetTextMetricsW(dc, &mut tm);
                    SelectObject(dc, old);
                }
                Measure { font, ascent: tm.tmAscent as f32 / MEASURE_EM }
            });
            f(dc, m)
        })
    })
}

/// Baseline of the first line below the box top, as a fraction of the size.
/// The PDF places text with this so it lands where GDI drew it.
pub fn ascent(style: &TextStyle) -> f32 {
    with_measure(style, |_, m| m.ascent)
}

/// Width in points of one line of text.
pub fn text_width(line: &str, style: &TextStyle) -> f32 {
    if line.is_empty() {
        return 0.0;
    }
    with_measure(style, |dc, m| gdi::text_width(dc, m.font, line) as f32 * style.size / MEASURE_EM)
}

/// Box (w, h) in points for multi-line text. Empty text keeps a sliver of
/// width so the box stays visible while typing.
pub fn text_size(text: &str, style: &TextStyle) -> (f32, f32) {
    let lines = text.split('\n');
    let w = lines.clone().map(|l| text_width(l, style)).fold(style.size * 0.4, f32::max);
    (w, lines.count() as f32 * LINE * style.size)
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

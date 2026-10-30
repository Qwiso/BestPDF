//! Overlays inside the PDF. On save each one becomes either a Stamp annotation
//! tagged /BestPDF (movable again when reopened, here or in other editors) or
//! plain page content (flattened). Opening reads the tagged stamps back and
//! removes them from the in-memory page, so they aren't drawn twice.
//!
//! Each overlay is built as page objects in its own "box space" (points,
//! origin at the box's bottom-left, upright as displayed), then mapped onto
//! the page with one matrix, which also takes care of rotated pages.

use std::ffi::{c_int, c_uint};

use std::collections::HashMap;

use crate::overlay::{self, Family, Image, Kind, Mark, MarkStyle, Overlay, TextStyle, LINE};
use crate::pdfium::{self, api, FPDF_DOCUMENT, FPDF_FONT, FPDF_PAGE, FPDF_PAGEOBJECT, FS_MATRIX, FS_RECTF};

/// Annotation key holding our metadata, e.g. "kind=text;size=14".
const KEY: &[u8] = b"BestPDF\0";
const CONTENTS: &[u8] = b"Contents\0";
/// DeviceToPage works in whole device pixels; this many per point keeps precision.
const K: f32 = 100.0;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

/// Maps between display points (top-left origin) and page space for one page.
pub(crate) struct PageMap {
    page: FPDF_PAGE,
    w: c_int,
    h: c_int,
}

impl PageMap {
    pub(crate) fn new(page: FPDF_PAGE, (pw, ph): (f32, f32)) -> Self {
        Self { page, w: (pw * K) as c_int, h: (ph * K) as c_int }
    }

    fn to_page(&self, x: f32, y: f32) -> (f64, f64) {
        let (mut px, mut py) = (0.0, 0.0);
        unsafe {
            (api().FPDF_DeviceToPage)(self.page, 0, 0, self.w, self.h, 0, (x * K) as c_int, (y * K) as c_int, &mut px, &mut py);
        }
        (px, py)
    }

    pub(crate) fn to_display(&self, px: f64, py: f64) -> (f32, f32) {
        let (mut x, mut y) = (0, 0);
        unsafe {
            (api().FPDF_PageToDevice)(self.page, 0, 0, self.w, self.h, 0, px, py, &mut x, &mut y);
        }
        (x as f32 / K, y as f32 / K)
    }

    /// Matrix from an overlay's box space to page space.
    fn box_matrix(&self, o: &Overlay) -> [f64; 6] {
        let (w, h) = (o.w.max(0.01) as f64, o.h.max(0.01) as f64);
        let p0 = self.to_page(o.x, o.y + o.h);
        let px = self.to_page(o.x + o.w, o.y + o.h);
        let py = self.to_page(o.x, o.y);
        [(px.0 - p0.0) / w, (px.1 - p0.1) / w, (py.0 - p0.0) / h, (py.1 - p0.1) / h, p0.0, p0.1]
    }

    /// The overlay's bounding rectangle in page space.
    fn page_rect(&self, o: &Overlay) -> FS_RECTF {
        let corners = [(o.x, o.y), (o.x + o.w, o.y), (o.x, o.y + o.h), (o.x + o.w, o.y + o.h)].map(|(x, y)| self.to_page(x, y));
        let xs = corners.map(|c| c.0 as f32);
        let ys = corners.map(|c| c.1 as f32);
        FS_RECTF {
            left: xs.iter().copied().fold(f32::MAX, f32::min),
            right: xs.iter().copied().fold(f32::MIN, f32::max),
            bottom: ys.iter().copied().fold(f32::MAX, f32::min),
            top: ys.iter().copied().fold(f32::MIN, f32::max),
        }
    }
}

/// Fonts loaded into one document while writing overlays, by
/// (family, bold, italic, embedded).
pub struct Fonts {
    doc: FPDF_DOCUMENT,
    loaded: HashMap<(Family, bool, bool, bool), FPDF_FONT>,
}

impl Fonts {
    pub fn new(doc: FPDF_DOCUMENT) -> Self {
        Self { doc, loaded: HashMap::new() }
    }

    /// The standard PDF font (built into every reader, nothing embedded) when
    /// the text fits its encoding; otherwise the matching Windows font, embedded.
    fn for_text(&mut self, text: &str, style: &TextStyle) -> FPDF_FONT {
        let a = api();
        let (doc, family, bold, italic) = (self.doc, style.family, style.bold, style.italic);
        let standard = |fonts: &mut Self| {
            *fonts.loaded.entry((family, bold, italic, false)).or_insert_with(|| unsafe {
                (a.FPDFText_LoadStandardFont)(doc, family.pdf_name(bold, italic).as_ptr())
            })
        };
        if text.chars().all(in_win_ansi) {
            return standard(self);
        }
        let embedded = *self.loaded.entry((family, bold, italic, true)).or_insert_with(|| {
            let windir = std::env::var_os("WINDIR").unwrap_or_else(|| "C:\\Windows".into());
            let path = std::path::Path::new(&windir).join("Fonts").join(family.ttf(bold, italic));
            std::fs::read(path)
                .map(|data| unsafe { (a.FPDFText_LoadFont)(doc, data.as_ptr(), data.len() as u32, pdfium::FPDF_FONT_TRUETYPE, 1) })
                .unwrap_or(std::ptr::null_mut())
        });
        if embedded.is_null() {
            standard(self)
        } else {
            embedded
        }
    }
}

impl Drop for Fonts {
    fn drop(&mut self) {
        for &f in self.loaded.values() {
            if !f.is_null() {
                unsafe { (api().FPDFFont_Close)(f) };
            }
        }
    }
}

/// Characters the standard fonts can show (Windows-1252).
fn in_win_ansi(c: char) -> bool {
    matches!(c as u32, 0x0A | 0x20..=0x7E | 0xA0..=0xFF) || "€‚ƒ„…†‡ˆ‰Š‹ŒŽ‘’“”•–—˜™š›œžŸ".contains(c)
}

/// Builds the page objects for one overlay, already placed on the page.
fn build(doc: FPDF_DOCUMENT, map: &PageMap, o: &Overlay, fonts: &mut Fonts) -> Vec<FPDF_PAGEOBJECT> {
    let a = api();
    let [ma, mb, mc, md, me, mf] = map.box_matrix(o);
    let mut objs = Vec::new();
    unsafe {
        match &o.kind {
            Kind::Mark(style) => {
                // Box space has its origin at the bottom-left, so flip the bar's top.
                let (y, h) = o.mark_bar().map_or((0.0, o.h), |(top, t)| (o.h - top - t, t));
                let r = (a.FPDFPageObj_CreateNewRect)(0.0, y, o.w, h);
                (a.FPDFPath_SetDrawMode)(r, pdfium::FPDF_FILLMODE_ALTERNATE, 0);
                let (cr, cg, cb) = rgb(style.color);
                (a.FPDFPageObj_SetFillColor)(r, cr, cg, cb, 255);
                if style.mark == Mark::Highlight {
                    (a.FPDFPageObj_SetBlendMode)(r, c"Multiply".as_ptr());
                }
                objs.push(r);
            }
            Kind::Text { text, style } => {
                let font = fonts.for_text(text, style);
                let size = style.size;
                let ascent = overlay::ascent(style);
                let (cr, cg, cb) = rgb(style.color);
                for (i, line) in text.split('\n').enumerate() {
                    if line.is_empty() {
                        continue;
                    }
                    let t = (a.FPDFPageObj_CreateTextObj)(doc, font, size);
                    if t.is_null() {
                        continue;
                    }
                    (a.FPDFText_SetText)(t, wide(line).as_ptr());
                    (a.FPDFPageObj_SetFillColor)(t, cr, cg, cb, 255);
                    let baseline = o.h - (ascent + i as f32 * LINE) * size;
                    (a.FPDFPageObj_Transform)(t, 1.0, 0.0, 0.0, 1.0, 0.0, baseline as f64);
                    objs.push(t);
                }
            }
            Kind::Image(img) => {
                let obj = (a.FPDFPageObj_NewImageObj)(doc);
                let mut pixels = img.bgra.clone();
                let bmp = (a.FPDFBitmap_CreateEx)(img.w, img.h, pdfium::FPDFBITMAP_BGRA, pixels.as_mut_ptr().cast(), img.w * 4);
                if !obj.is_null() && !bmp.is_null() {
                    (a.FPDFImageObj_SetBitmap)(std::ptr::null_mut(), 0, obj, bmp);
                    (a.FPDFImageObj_SetMatrix)(obj, o.w as f64, 0.0, 0.0, o.h as f64, 0.0, 0.0);
                    objs.push(obj);
                }
                if !bmp.is_null() {
                    (a.FPDFBitmap_Destroy)(bmp);
                }
            }
        }
        for &obj in &objs {
            (a.FPDFPageObj_Transform)(obj, ma, mb, mc, md, me, mf);
        }
    }
    objs
}

fn rgb(color: u32) -> (c_uint, c_uint, c_uint) {
    ((color >> 16) & 0xFF, (color >> 8) & 0xFF, color & 0xFF)
}

fn meta(o: &Overlay) -> String {
    match &o.kind {
        Kind::Text { style: s, .. } => format!(
            "kind=text;size={};font={};bold={};italic={};color={:06x}",
            s.size,
            s.family.key(),
            s.bold as u8,
            s.italic as u8,
            s.color
        ),
        Kind::Mark(s) => format!("kind=mark;mark={};color={:06x}", s.mark.key(), s.color),
        Kind::Image(_) => "kind=image".into(),
    }
}

/// Writes overlays onto a loaded page, as tagged stamps or flattened content.
pub fn write(doc: FPDF_DOCUMENT, page: FPDF_PAGE, size: (f32, f32), overlays: &[&Overlay], flatten: bool, fonts: &mut Fonts) {
    let a = api();
    let map = PageMap::new(page, size);
    for o in overlays {
        let objs = build(doc, &map, o, fonts);
        unsafe {
            if flatten {
                for obj in objs {
                    (a.FPDFPage_InsertObject)(page, obj);
                }
                continue;
            }
            let annot = (a.FPDFPage_CreateAnnot)(page, pdfium::FPDF_ANNOT_STAMP);
            if annot.is_null() {
                continue;
            }
            (a.FPDFAnnot_SetRect)(annot, &map.page_rect(o));
            for obj in objs {
                (a.FPDFAnnot_AppendObject)(annot, obj);
            }
            (a.FPDFAnnot_SetFlags)(annot, pdfium::FPDF_ANNOT_FLAG_PRINT);
            (a.FPDFAnnot_SetStringValue)(annot, KEY.as_ptr().cast(), wide(&meta(o)).as_ptr());
            if let Kind::Text { text, .. } = &o.kind {
                (a.FPDFAnnot_SetStringValue)(annot, CONTENTS.as_ptr().cast(), wide(text).as_ptr());
            }
            (a.FPDFPage_CloseAnnot)(annot);
        }
    }
    if flatten && !overlays.is_empty() {
        unsafe { (a.FPDFPage_GenerateContent)(page) };
    }
}

fn string_value(annot: pdfium::FPDF_ANNOTATION, key: &[u8]) -> Option<String> {
    let a = api();
    let bytes = unsafe { (a.FPDFAnnot_GetStringValue)(annot, key.as_ptr().cast(), std::ptr::null_mut(), 0) } as usize;
    if bytes <= 2 {
        return None;
    }
    let mut buf = vec![0u16; bytes / 2];
    unsafe { (a.FPDFAnnot_GetStringValue)(annot, key.as_ptr().cast(), buf.as_mut_ptr(), bytes as _) };
    buf.pop(); // NUL
    Some(String::from_utf16_lossy(&buf))
}

/// Removes page `index`'s Best PDF stamps from the loaded page and returns them.
pub fn take(doc: FPDF_DOCUMENT, page: FPDF_PAGE, index: usize, size: (f32, f32)) -> Vec<Overlay> {
    let a = api();
    let map = PageMap::new(page, size);
    let mut found = Vec::new();
    let count = unsafe { (a.FPDFPage_GetAnnotCount)(page) };
    for i in (0..count).rev() {
        unsafe {
            let annot = (a.FPDFPage_GetAnnot)(page, i);
            if annot.is_null() {
                continue;
            }
            let Some(meta) = string_value(annot, KEY) else {
                (a.FPDFPage_CloseAnnot)(annot);
                continue;
            };
            let field = |name: &str| meta.split(';').find_map(|kv| kv.strip_prefix(name)?.strip_prefix('='));
            let mut rect = FS_RECTF::default();
            (a.FPDFAnnot_GetRect)(annot, &mut rect);
            let (x0, y0) = map.to_display(rect.left as f64, rect.top as f64);
            let (x1, y1) = map.to_display(rect.right as f64, rect.bottom as f64);
            let (x, y, w, h) = (x0.min(x1), y0.min(y1), (x1 - x0).abs(), (y1 - y0).abs());
            let color = field("color").and_then(|c| u32::from_str_radix(c, 16).ok());
            let overlay = match field("kind") {
                Some("text") => {
                    let flag = |name: &str| field(name) == Some("1");
                    let style = TextStyle {
                        family: Family::from_key(field("font").unwrap_or("sans")),
                        size: field("size").and_then(|s| s.parse().ok()).unwrap_or(12.0),
                        bold: flag("bold"),
                        italic: flag("italic"),
                        color: color.unwrap_or(0x000000),
                    };
                    let text = string_value(annot, CONTENTS).unwrap_or_default().replace("\r\n", "\n").replace('\r', "\n");
                    Some(Overlay::text(index, x, y, text, style))
                }
                Some("mark") => {
                    let style = MarkStyle { mark: Mark::from_key(field("mark").unwrap_or("")), color: color.unwrap_or(overlay::MARK_COLORS[0]) };
                    Some(Overlay { page: index, x, y, w, h, kind: Kind::Mark(style) })
                }
                // Written by the first version: pure yellow highlights.
                Some("highlight") => {
                    Some(Overlay { page: index, x, y, w, h, kind: Kind::Mark(MarkStyle { mark: Mark::Highlight, color: 0xFFFF00 }) })
                }
                Some("image") => read_image(doc, page, annot).map(|img| Overlay { page: index, x, y, w, h, kind: Kind::Image(img) }),
                _ => None,
            };
            (a.FPDFPage_CloseAnnot)(annot);
            if let Some(o) = overlay {
                (a.FPDFPage_RemoveAnnot)(page, i);
                found.push(o);
            }
        }
    }
    found.reverse();
    found
}

/// The stamp's image at its full resolution, alpha included.
unsafe fn read_image(doc: FPDF_DOCUMENT, page: FPDF_PAGE, annot: pdfium::FPDF_ANNOTATION) -> Option<std::rc::Rc<Image>> {
    let a = api();
    let obj = (0..(a.FPDFAnnot_GetObjectCount)(annot))
        .map(|k| (a.FPDFAnnot_GetObject)(annot, k))
        .find(|&o| !o.is_null() && (a.FPDFPageObj_GetType)(o) == pdfium::FPDF_PAGEOBJ_IMAGE)?;
    let (mut pw, mut ph) = (0, 0);
    if (a.FPDFImageObj_GetImagePixelSize)(obj, &mut pw, &mut ph) == 0 || pw == 0 || ph == 0 {
        return None;
    }
    // Rendered bitmaps come out at the object's size in points: make that the pixel size.
    (a.FPDFPageObj_SetMatrix)(obj, &FS_MATRIX { a: pw as f32, b: 0.0, c: 0.0, d: ph as f32, e: 0.0, f: 0.0 });
    let bmp = (a.FPDFImageObj_GetRenderedBitmap)(doc, page, obj);
    if bmp.is_null() {
        return None;
    }
    let (w, h, stride) = ((a.FPDFBitmap_GetWidth)(bmp), (a.FPDFBitmap_GetHeight)(bmp), (a.FPDFBitmap_GetStride)(bmp));
    let format = (a.FPDFBitmap_GetFormat)(bmp);
    let src = (a.FPDFBitmap_GetBuffer)(bmp) as *const u8;
    let mut bgra = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        let row = std::slice::from_raw_parts(src.add((y * stride) as usize), stride as usize);
        for x in 0..w as usize {
            match format {
                pdfium::FPDFBITMAP_BGRA => bgra.extend_from_slice(&row[x * 4..x * 4 + 4]),
                pdfium::FPDFBITMAP_BGRX => bgra.extend_from_slice(&[row[x * 4], row[x * 4 + 1], row[x * 4 + 2], 255]),
                2 => bgra.extend_from_slice(&[row[x * 3], row[x * 3 + 1], row[x * 3 + 2], 255]),
                _ => bgra.extend_from_slice(&[row[x], row[x], row[x], 255]),
            }
        }
    }
    (a.FPDFBitmap_Destroy)(bmp);
    Some(Image::new(w, h, bgra))
}

//! The signature library: signatures and initials saved as PNGs in a
//! `signatures` folder (see settings::data_dir), plus turning pen strokes or an
//! imported picture into a clean transparent image.

use std::path::PathBuf;
use std::rc::Rc;

use windows::Win32::Graphics::Gdi::*;

use crate::imageio;
use crate::overlay::Image;
use crate::settings;

#[derive(Clone, Copy, PartialEq)]
pub enum SigKind {
    Signature,
    Initials,
}

impl SigKind {
    fn prefix(self) -> &'static str {
        match self {
            SigKind::Signature => "signature",
            SigKind::Initials => "initials",
        }
    }

    /// Height in points when first stamped onto a page.
    pub fn stamp_height(self) -> f32 {
        match self {
            SigKind::Signature => 36.0,
            SigKind::Initials => 24.0,
        }
    }

    /// Widest it starts out, in points.
    pub fn stamp_max_width(self) -> f32 {
        match self {
            SigKind::Signature => 180.0,
            SigKind::Initials => 72.0,
        }
    }
}

pub struct Sig {
    pub kind: SigKind,
    pub path: PathBuf,
    pub image: Rc<Image>,
}

impl Sig {
    /// Starting size on the page, in points.
    pub fn stamp_size(&self) -> (f32, f32) {
        let aspect = self.image.w as f32 / self.image.h.max(1) as f32;
        let h = self.kind.stamp_height().min(self.kind.stamp_max_width() / aspect);
        (h * aspect, h)
    }
}

#[derive(Default)]
pub struct Library {
    pub items: Vec<Sig>,
}

fn dir() -> PathBuf {
    settings::data_dir().join("signatures")
}

impl Library {
    pub fn load() -> Self {
        let mut paths: Vec<PathBuf> = std::fs::read_dir(dir())
            .map(|rd| rd.filter_map(|e| Some(e.ok()?.path())).collect())
            .unwrap_or_default();
        paths.sort(); // names carry the creation time
        let items = paths
            .into_iter()
            .filter_map(|path| {
                let name = path.file_name()?.to_str()?.to_ascii_lowercase();
                if !name.ends_with(".png") {
                    return None;
                }
                let kind = [SigKind::Signature, SigKind::Initials].into_iter().find(|k| name.starts_with(k.prefix()))?;
                Some(Sig { kind, image: imageio::load(&path).ok()?, path })
            })
            .collect();
        Self { items }
    }

    /// Saves a new signature or initials; returns its index.
    pub fn add(&mut self, kind: SigKind, image: Rc<Image>) -> Result<usize, String> {
        let dir = dir();
        std::fs::create_dir_all(&dir).map_err(|e| format!("Could not create {}: {e}", dir.display()))?;
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis());
        let path = dir.join(format!("{}-{stamp}.png", kind.prefix()));
        imageio::save(&path, image.w, image.h, &image.bgra, imageio::Format::Png)?;
        self.items.push(Sig { kind, path, image });
        Ok(self.items.len() - 1)
    }

    pub fn remove(&mut self, i: usize) -> Result<(), String> {
        let Some(sig) = self.items.get(i) else { return Ok(()) };
        std::fs::remove_file(&sig.path).map_err(|e| format!("Could not delete {}: {e}", sig.path.display()))?;
        self.items.remove(i);
        Ok(())
    }
}

// ---- making signature images -----------------------------------------------

/// Pen color for drawn signatures.
const INK: [u8; 3] = [0x1A, 0x1A, 0x1A]; // B, G, R
/// Drawn signatures are rendered this many pixels tall (before cropping).
const DRAW_H: i32 = 300;
/// Supersampling factor for smooth edges.
const SS: i32 = 3;
/// Pen width as a fraction of the canvas height.
pub const PEN: f32 = 0.022;

/// Renders pen strokes (points in 0..1 canvas coordinates) on a canvas of
/// the given width/height ratio. None if nothing was drawn.
pub fn rasterize(strokes: &[Vec<(f32, f32)>], aspect: f32) -> Option<Rc<Image>> {
    if strokes.iter().all(|s| s.is_empty()) {
        return None;
    }
    let (w, h) = ((DRAW_H as f32 * aspect) as i32, DRAW_H);
    let (bw, bh) = (w * SS, h * SS);
    let mut gray = vec![0u8; (w * h) as usize];
    unsafe {
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: bw,
                biHeight: -bh,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let bitmap = CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
        let dc = CreateCompatibleDC(None);
        let old_bmp = SelectObject(dc, bitmap.into());
        std::ptr::write_bytes(bits as *mut u8, 0xFF, (bw * bh * 4) as usize);
        let brush = LOGBRUSH { lbStyle: BS_SOLID, lbColor: crate::theme::rgb(0), lbHatch: 0 };
        let pen_w = (PEN * bh as f32).round() as u32;
        let pen = ExtCreatePen(PS_GEOMETRIC | PS_SOLID | PS_ENDCAP_ROUND | PS_JOIN_ROUND, pen_w, &brush, None);
        let old_pen = SelectObject(dc, pen.into());
        for stroke in strokes {
            let mut pts: Vec<windows::Win32::Foundation::POINT> = stroke
                .iter()
                .map(|&(x, y)| windows::Win32::Foundation::POINT { x: (x * bw as f32) as i32, y: (y * bh as f32) as i32 })
                .collect();
            if pts.len() == 1 {
                // A dot: round caps on a tiny segment.
                pts.push(windows::Win32::Foundation::POINT { x: pts[0].x + 1, y: pts[0].y });
            }
            let _ = Polyline(dc, &pts);
        }
        let _ = GdiFlush();
        let px = std::slice::from_raw_parts(bits as *const u8, (bw * bh * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                let mut sum = 0u32;
                for sy in 0..SS {
                    for sx in 0..SS {
                        sum += px[(((y * SS + sy) * bw + x * SS + sx) * 4 + 1) as usize] as u32;
                    }
                }
                gray[(y * w + x) as usize] = (sum / (SS * SS) as u32) as u8;
            }
        }
        SelectObject(dc, old_pen);
        SelectObject(dc, old_bmp);
        let _ = DeleteObject(pen.into());
        let _ = DeleteDC(dc);
        let _ = DeleteObject(bitmap.into());
    }
    let bgra = gray.iter().flat_map(|&g| [INK[0], INK[1], INK[2], 255 - g]).collect();
    crop(&Image::new(w, h, bgra))
}

/// Prepares an imported picture: a photo or scan of ink on paper gets its
/// paper made transparent; pictures that already have transparency are kept.
/// Then trimmed to the ink and shrunk to a sensible size.
pub fn prepare_import(img: &Image) -> Option<Rc<Image>> {
    let mut bgra = img.bgra.clone();
    let has_alpha = bgra.chunks_exact(4).any(|p| p[3] < 250);
    if !has_alpha {
        for p in bgra.chunks_exact_mut(4) {
            let lum = (p[2] as f32 * 0.299 + p[1] as f32 * 0.587 + p[0] as f32 * 0.114).round();
            // Paper (light) → clear, ink (dark) → solid, with a soft ramp between.
            let a = ((235.0 - lum) * 255.0 / 175.0).clamp(0.0, 255.0);
            if a > 0.0 {
                // Undo the blend with white paper to recover the ink color.
                let k = a / 255.0;
                for c in 0..3 {
                    p[c] = ((p[c] as f32 - (1.0 - k) * 255.0) / k).clamp(0.0, 255.0) as u8;
                }
            }
            p[3] = a as u8;
        }
    }
    let img = crop(&Image::new(img.w, img.h, bgra))?;
    const MAX_W: i32 = 1200;
    Some(if img.w > MAX_W { img.resized(MAX_W, (img.h as i64 * MAX_W as i64 / img.w as i64) as i32) } else { img })
}

/// Trims transparent margins, keeping a little padding. None if fully clear.
fn crop(img: &Image) -> Option<Rc<Image>> {
    let (w, h) = (img.w as usize, img.h as usize);
    let solid = |x: usize, y: usize| img.bgra[(y * w + x) * 4 + 3] > 8;
    let rows: Vec<usize> = (0..h).filter(|&y| (0..w).any(|x| solid(x, y))).collect();
    let cols: Vec<usize> = (0..w).filter(|&x| (0..h).any(|y| solid(x, y))).collect();
    let (&top, &bottom, &left, &right) = (rows.first()?, rows.last()?, cols.first()?, cols.last()?);
    let pad = (w.max(h) / 50).max(4);
    let (x0, y0) = (left.saturating_sub(pad), top.saturating_sub(pad));
    let (x1, y1) = ((right + pad + 1).min(w), (bottom + pad + 1).min(h));
    let mut out = Vec::with_capacity((x1 - x0) * (y1 - y0) * 4);
    for y in y0..y1 {
        out.extend_from_slice(&img.bgra[(y * w + x0) * 4..(y * w + x1) * 4]);
    }
    Some(Image::new((x1 - x0) as i32, (y1 - y0) as i32, out))
}

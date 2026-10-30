//! The sheet for making a new signature or initials: draw with the mouse on a
//! white canvas, or import a picture, then save it to the library.

use std::rc::Rc;

use windows::Win32::Foundation::{POINT, RECT};
use windows::Win32::Graphics::Gdi::*;

use crate::gdi::{self, rect, Tint};
use crate::overlay::Image;
use crate::signature::{self, SigKind};
use crate::theme::{self as t, Metrics};

/// Canvas width / height.
const ASPECT: f32 = 3.0;

#[derive(Clone, Copy, PartialEq)]
pub enum PadHit {
    None,
    Canvas,
    Import,
    Clear,
    Cancel,
    Save,
}

pub enum PadEvent {
    None,
    Redraw,
    /// Started drawing a stroke: capture the mouse.
    Capture,
    /// Finished a stroke.
    Release,
    Import,
    Cancel,
    Save(Rc<Image>),
}

struct Layout {
    panel: RECT,
    title: RECT,
    canvas: RECT,
    buttons: [(PadHit, RECT); 4],
}

pub struct SignPad {
    pub kind: SigKind,
    /// Pen strokes in 0..1 canvas coordinates.
    strokes: Vec<Vec<(f32, f32)>>,
    image: Option<Rc<Image>>,
    drawing: bool,
    hover: PadHit,
}

impl SignPad {
    pub fn new(kind: SigKind) -> Self {
        Self { kind, strokes: Vec::new(), image: None, drawing: false, hover: PadHit::None }
    }

    /// Shows an imported picture instead of any strokes.
    pub fn set_image(&mut self, img: Rc<Image>) {
        self.strokes.clear();
        self.image = Some(img);
    }

    fn is_empty(&self) -> bool {
        self.image.is_none() && self.strokes.is_empty()
    }

    fn layout(m: Metrics, (cw, ch): (i32, i32)) -> Layout {
        let pad = m.px(20);
        let w = m.px(620).min(cw - m.px(32)).max(m.px(240));
        let canvas_w = w - 2 * pad;
        let canvas_h = (canvas_w as f32 / ASPECT) as i32;
        let (title_h, button_h, gap) = (m.px(44), m.px(30), m.px(16));
        let h = pad + title_h + canvas_h + gap + button_h + pad;
        let panel = rect((cw - w) / 2, ((ch - h) / 2).max(0), w, h);
        let title = rect(panel.left + pad, panel.top + pad, canvas_w, title_h);
        let canvas = rect(panel.left + pad, title.bottom, canvas_w, canvas_h);
        let by = canvas.bottom + gap;
        let bw = m.px(100);
        let bgap = m.px(8);
        let right = panel.right - pad;
        Layout {
            panel,
            title,
            canvas,
            buttons: [
                (PadHit::Import, rect(canvas.left, by, m.px(130), button_h)),
                (PadHit::Clear, rect(canvas.left + m.px(130) + bgap, by, bw, button_h)),
                (PadHit::Cancel, rect(right - 2 * bw - bgap, by, bw, button_h)),
                (PadHit::Save, rect(right - bw, by, bw, button_h)),
            ],
        }
    }

    pub fn hit(&self, m: Metrics, size: (i32, i32), x: i32, y: i32) -> PadHit {
        let l = Self::layout(m, size);
        if gdi::contains(&l.canvas, x, y) {
            return PadHit::Canvas;
        }
        l.buttons.iter().find(|(_, r)| gdi::contains(r, x, y)).map_or(PadHit::None, |&(h, _)| h)
    }

    fn canvas_point(m: Metrics, size: (i32, i32), x: i32, y: i32) -> (f32, f32) {
        let c = Self::layout(m, size).canvas;
        (
            ((x - c.left) as f32 / (c.right - c.left) as f32).clamp(0.0, 1.0),
            ((y - c.top) as f32 / (c.bottom - c.top) as f32).clamp(0.0, 1.0),
        )
    }

    pub fn mouse_move(&mut self, m: Metrics, size: (i32, i32), x: i32, y: i32) -> bool {
        if self.drawing {
            let p = Self::canvas_point(m, size, x, y);
            if let Some(stroke) = self.strokes.last_mut() {
                if stroke.last() != Some(&p) {
                    stroke.push(p);
                    return true;
                }
            }
            return false;
        }
        let hit = self.hit(m, size, x, y);
        let changed = hit != self.hover;
        self.hover = hit;
        changed
    }

    pub fn left_down(&mut self, m: Metrics, size: (i32, i32), x: i32, y: i32) -> PadEvent {
        match self.hit(m, size, x, y) {
            PadHit::Canvas => {
                self.image = None; // drawing replaces an imported picture
                self.strokes.push(vec![Self::canvas_point(m, size, x, y)]);
                self.drawing = true;
                PadEvent::Capture
            }
            PadHit::Import => PadEvent::Import,
            PadHit::Clear => {
                self.strokes.clear();
                self.image = None;
                PadEvent::Redraw
            }
            PadHit::Cancel => PadEvent::Cancel,
            PadHit::Save => match self.image.clone().or_else(|| signature::rasterize(&self.strokes, ASPECT)) {
                Some(img) => PadEvent::Save(img),
                None => PadEvent::None,
            },
            PadHit::None => PadEvent::None,
        }
    }

    pub fn left_up(&mut self) -> PadEvent {
        if std::mem::take(&mut self.drawing) {
            return PadEvent::Release;
        }
        PadEvent::None
    }

    pub fn capture_lost(&mut self) {
        self.drawing = false;
    }

    pub fn paint(&self, dc: HDC, m: Metrics, size: (i32, i32), tint: &Tint, ui: HFONT, bold: HFONT) {
        let l = Self::layout(m, size);
        tint.fill(dc, &rect(0, 0, size.0, size.1), t::rgb(0x000000), 0x99);
        gdi::fill(dc, &l.panel, t::BORDER);
        gdi::fill(dc, &RECT { left: l.panel.left + 1, top: l.panel.top + 1, right: l.panel.right - 1, bottom: l.panel.bottom - 1 }, t::CHROME_BG);

        let (what, hint) = match self.kind {
            SigKind::Signature => ("New signature", "Draw it below, or import a picture of it"),
            SigKind::Initials => ("New initials", "Draw them below, or import a picture of them"),
        };
        let half = (l.title.bottom - l.title.top) / 2;
        gdi::text(dc, bold, t::TEXT_BRIGHT, what, RECT { bottom: l.title.top + half, ..l.title }, DT_LEFT | DT_VCENTER);
        gdi::text(dc, ui, t::TEXT_DIM, hint, RECT { top: l.title.top + half, bottom: l.title.bottom - m.px(6), ..l.title }, DT_LEFT | DT_VCENTER);

        // Canvas: paper, a signing line, then the ink.
        let c = l.canvas;
        let (cw, ch) = (c.right - c.left, c.bottom - c.top);
        gdi::fill(dc, &c, t::WHITE);
        let line_y = c.top + ch * 3 / 4;
        gdi::fill(dc, &rect(c.left + m.px(24), line_y, cw - m.px(48), 1), t::rgb(0xC8C8C8));
        gdi::text(dc, ui, t::rgb(0xB0B0B0), "×", rect(c.left + m.px(8), line_y - m.px(20), m.px(16), m.px(18)), DT_LEFT | DT_VCENTER);
        if self.is_empty() {
            gdi::text(dc, ui, t::rgb(0xB0B0B0), "Sign here", rect(c.left, line_y - m.px(40), cw, m.px(20)), DT_CENTER | DT_VCENTER);
        }
        if let Some(img) = &self.image {
            // Fit inside the canvas with a margin, keeping proportions.
            let margin = m.px(12);
            let (aw, ah) = ((cw - 2 * margin) as f32, (ch - 2 * margin) as f32);
            let k = (aw / img.w as f32).min(ah / img.h as f32).min(1.0 * m.dpi as f32 / 96.0);
            let (w, h) = ((img.w as f32 * k) as i32, (img.h as f32 * k) as i32);
            img.draw(dc, &rect(c.left + (cw - w) / 2, c.top + (ch - h) / 2, w, h), 255);
        }
        unsafe {
            let brush = LOGBRUSH { lbStyle: BS_SOLID, lbColor: t::rgb(0x1A1A1A), lbHatch: 0 };
            let pen_w = ((signature::PEN * ch as f32).round() as u32).max(1);
            let pen = ExtCreatePen(PS_GEOMETRIC | PS_SOLID | PS_ENDCAP_ROUND | PS_JOIN_ROUND, pen_w, &brush, None);
            let old = SelectObject(dc, pen.into());
            for stroke in &self.strokes {
                let mut pts: Vec<POINT> = stroke
                    .iter()
                    .map(|&(x, y)| POINT { x: c.left + (x * cw as f32) as i32, y: c.top + (y * ch as f32) as i32 })
                    .collect();
                if pts.len() == 1 {
                    pts.push(POINT { x: pts[0].x + 1, y: pts[0].y });
                }
                let _ = Polyline(dc, &pts);
            }
            SelectObject(dc, old);
            let _ = DeleteObject(pen.into());
        }

        for (hit, r) in l.buttons {
            let label = match hit {
                PadHit::Import => "Import image…",
                PadHit::Clear => "Clear",
                PadHit::Cancel => "Cancel",
                _ => "Save",
            };
            let hovered = self.hover == hit;
            let (bg, fg) = match hit {
                PadHit::Save if self.is_empty() => (t::KEY_BG, t::TEXT_FAINT),
                PadHit::Save => (if hovered { t::rgb(0x026EC1) } else { t::ACCENT }, t::TEXT_BRIGHT),
                PadHit::Clear if self.is_empty() => (t::KEY_BG, t::TEXT_FAINT),
                _ => (if hovered { t::rgb(0x3C3C3C) } else { t::KEY_BG }, t::TEXT),
            };
            gdi::round_fill(dc, &r, m.px(4), bg);
            gdi::text(dc, ui, fg, label, r, DT_CENTER | DT_VCENTER);
        }
    }
}

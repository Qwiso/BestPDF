//! A small modal form drawn over the window: a title, an optional message,
//! some fields (text boxes, checkboxes, choices) and Cancel / OK. Used for Go
//! to Page, Extract and Split, the password prompt, and export options.

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;

use crate::gdi::{self, rect, Tint};
use crate::theme::{self as t, Metrics};

pub enum Field {
    Text { label: &'static str, value: String, hint: &'static str, secret: bool },
    Check { label: &'static str, on: bool },
    Choice { label: &'static str, options: Vec<&'static str>, index: usize },
}

#[derive(Clone, Copy, PartialEq)]
pub enum SheetHit {
    Field(usize),
    Option(usize, usize),
    Ok,
    Cancel,
}

pub enum SheetEvent {
    None,
    Redraw,
    Ok,
    Cancel,
}

pub struct Sheet {
    pub title: String,
    /// Shown under the title; `error` makes it red.
    pub message: Option<String>,
    pub error: bool,
    pub fields: Vec<Field>,
    pub ok_label: &'static str,
    /// The text field typing goes into.
    focus: Option<usize>,
    hover: Option<SheetHit>,
}

struct Layout {
    panel: RECT,
    title: RECT,
    message: RECT,
    fields: Vec<(RECT, RECT)>, // (label, control)
    ok: RECT,
    cancel: RECT,
}

impl Sheet {
    pub fn new(title: impl Into<String>, fields: Vec<Field>, ok_label: &'static str) -> Self {
        let focus = fields.iter().position(|f| matches!(f, Field::Text { .. }));
        Self { title: title.into(), message: None, error: false, fields, ok_label, focus, hover: None }
    }

    pub fn with_message(mut self, message: impl Into<String>, error: bool) -> Self {
        self.message = Some(message.into());
        self.error = error;
        self
    }

    pub fn text(&self, i: usize) -> &str {
        match self.fields.get(i) {
            Some(Field::Text { value, .. }) => value,
            _ => "",
        }
    }

    pub fn checked(&self, i: usize) -> bool {
        matches!(self.fields.get(i), Some(Field::Check { on: true, .. }))
    }

    pub fn choice(&self, i: usize) -> usize {
        match self.fields.get(i) {
            Some(Field::Choice { index, .. }) => *index,
            _ => 0,
        }
    }

    fn layout(&self, m: Metrics, (cw, ch): (i32, i32)) -> Layout {
        let pad = m.px(20);
        let w = m.px(480).min(cw - m.px(32)).max(m.px(280));
        let row = m.px(38);
        let title_h = m.px(30);
        let message_h = if self.message.is_some() { m.px(26) } else { 0 };
        let button_h = m.px(30);
        let h = pad + title_h + message_h + m.px(8) + row * self.fields.len() as i32 + m.px(12) + button_h + pad;
        let panel = rect((cw - w) / 2, ((ch - h) / 3).max(0), w, h);
        let inner_w = w - 2 * pad;
        let title = rect(panel.left + pad, panel.top + pad, inner_w, title_h);
        let message = rect(panel.left + pad, title.bottom, inner_w, message_h);
        let label_w = m.px(150);
        let mut y = message.bottom + m.px(8);
        let fields = self
            .fields
            .iter()
            .map(|_| {
                let label = rect(panel.left + pad, y, label_w, row);
                let control = rect(panel.left + pad + label_w, y + m.px(4), inner_w - label_w, row - m.px(8));
                y += row;
                (label, control)
            })
            .collect();
        let bw = m.px(100);
        let by = y + m.px(12);
        let ok = rect(panel.right - pad - bw, by, bw, button_h);
        let cancel = rect(ok.left - m.px(8) - bw, by, bw, button_h);
        Layout { panel, title, message, fields, ok, cancel }
    }

    /// Option buttons of a choice field, side by side across its control.
    fn option_rects(control: &RECT, n: usize, m: Metrics) -> Vec<RECT> {
        let gap = m.px(4);
        let w = ((control.right - control.left) - gap * (n as i32 - 1)) / n.max(1) as i32;
        (0..n).map(|k| rect(control.left + k as i32 * (w + gap), control.top, w, control.bottom - control.top)).collect()
    }

    fn hit(&self, m: Metrics, size: (i32, i32), x: i32, y: i32) -> Option<SheetHit> {
        let l = self.layout(m, size);
        if gdi::contains(&l.ok, x, y) {
            return Some(SheetHit::Ok);
        }
        if gdi::contains(&l.cancel, x, y) {
            return Some(SheetHit::Cancel);
        }
        for (i, (field, (label, control))) in self.fields.iter().zip(&l.fields).enumerate() {
            match field {
                Field::Choice { options, .. } => {
                    if let Some(k) = Self::option_rects(control, options.len(), m).iter().position(|r| gdi::contains(r, x, y)) {
                        return Some(SheetHit::Option(i, k));
                    }
                }
                Field::Check { .. } if gdi::contains(&RECT { left: label.left, ..*control }, x, y) => return Some(SheetHit::Field(i)),
                Field::Text { .. } if gdi::contains(control, x, y) => return Some(SheetHit::Field(i)),
                _ => {}
            }
        }
        None
    }

    pub fn is_hand(&self, m: Metrics, size: (i32, i32), x: i32, y: i32) -> bool {
        matches!(self.hit(m, size, x, y), Some(SheetHit::Ok | SheetHit::Cancel | SheetHit::Option(..) | SheetHit::Field(_)))
    }

    pub fn mouse_move(&mut self, m: Metrics, size: (i32, i32), x: i32, y: i32) -> bool {
        let hit = self.hit(m, size, x, y);
        let changed = hit != self.hover;
        self.hover = hit;
        changed
    }

    pub fn left_down(&mut self, m: Metrics, size: (i32, i32), x: i32, y: i32) -> SheetEvent {
        match self.hit(m, size, x, y) {
            Some(SheetHit::Ok) => SheetEvent::Ok,
            Some(SheetHit::Cancel) => SheetEvent::Cancel,
            Some(SheetHit::Option(i, k)) => {
                if let Some(Field::Choice { index, .. }) = self.fields.get_mut(i) {
                    *index = k;
                }
                SheetEvent::Redraw
            }
            Some(SheetHit::Field(i)) => {
                match self.fields.get_mut(i) {
                    Some(Field::Check { on, .. }) => *on = !*on,
                    Some(Field::Text { .. }) => self.focus = Some(i),
                    _ => {}
                }
                SheetEvent::Redraw
            }
            None => SheetEvent::None,
        }
    }

    pub fn key(&mut self, key: VIRTUAL_KEY) -> SheetEvent {
        match key {
            VK_RETURN => SheetEvent::Ok,
            VK_ESCAPE => SheetEvent::Cancel,
            VK_TAB => {
                // Next text field.
                let texts: Vec<usize> = (0..self.fields.len()).filter(|&i| matches!(self.fields[i], Field::Text { .. })).collect();
                if let Some(pos) = self.focus.and_then(|f| texts.iter().position(|&i| i == f)) {
                    self.focus = Some(texts[(pos + 1) % texts.len()]);
                }
                SheetEvent::Redraw
            }
            _ => SheetEvent::None,
        }
    }

    pub fn char_input(&mut self, c: char) -> SheetEvent {
        let Some(Field::Text { value, .. }) = self.focus.and_then(|i| self.fields.get_mut(i)) else { return SheetEvent::None };
        match c {
            '\u{8}' => {
                value.pop();
            }
            c if c.is_control() => return SheetEvent::None,
            c => value.push(c),
        }
        SheetEvent::Redraw
    }

    pub fn paint(&self, dc: HDC, m: Metrics, size: (i32, i32), tint: &Tint, ui: HFONT, bold: HFONT, small: HFONT) {
        let l = self.layout(m, size);
        tint.fill(dc, &rect(0, 0, size.0, size.1), t::rgb(0x000000), 0x99);
        gdi::fill(dc, &l.panel, t::BORDER);
        gdi::fill(dc, &RECT { left: l.panel.left + 1, top: l.panel.top + 1, right: l.panel.right - 1, bottom: l.panel.bottom - 1 }, t::CHROME_BG);
        gdi::text(dc, bold, t::TEXT_BRIGHT, &self.title, l.title, DT_LEFT | DT_VCENTER | DT_END_ELLIPSIS);
        if let Some(message) = &self.message {
            let color = if self.error { t::PDF_ICON } else { t::TEXT_DIM };
            gdi::text(dc, small, color, message, l.message, DT_LEFT | DT_VCENTER | DT_END_ELLIPSIS);
        }
        for (i, (field, &(label, control))) in self.fields.iter().zip(&l.fields).enumerate() {
            match field {
                Field::Text { label: name, value, hint, secret } => {
                    gdi::text(dc, ui, t::TEXT, name, label, DT_LEFT | DT_VCENTER);
                    gdi::fill(dc, &control, t::rgb(0x313131));
                    let focused = self.focus == Some(i);
                    if focused {
                        frame(dc, &control, t::ACCENT);
                    }
                    let shown = if *secret { "•".repeat(value.chars().count()) } else { value.clone() };
                    let inner = RECT { left: control.left + m.px(8), right: control.right - m.px(6), ..control };
                    if shown.is_empty() {
                        gdi::text(dc, small, t::TEXT_FAINT, hint, inner, DT_LEFT | DT_VCENTER | DT_END_ELLIPSIS);
                    } else {
                        gdi::text(dc, ui, t::TEXT_BRIGHT, &shown, inner, DT_LEFT | DT_VCENTER | DT_END_ELLIPSIS);
                    }
                    if focused {
                        let measure = unsafe { CreateCompatibleDC(None) };
                        let x = (inner.left + gdi::text_width(measure, ui, &shown) + 1).min(inner.right);
                        unsafe {
                            let _ = DeleteDC(measure);
                        }
                        gdi::fill(dc, &rect(x, control.top + m.px(6), m.px(1).max(1), control.bottom - control.top - m.px(12)), t::TEXT_BRIGHT);
                    }
                }
                Field::Check { label: name, on } => {
                    let b = m.px(16);
                    let boxr = rect(label.left, (label.top + label.bottom - b) / 2, b, b);
                    gdi::fill(dc, &boxr, if *on { t::ACCENT } else { t::rgb(0x3C3C3C) });
                    if *on {
                        gdi::text(dc, small, t::TEXT_BRIGHT, "\u{2713}", boxr, DT_CENTER | DT_VCENTER);
                    }
                    let text = RECT { left: boxr.right + m.px(8), right: control.right, ..label };
                    gdi::text(dc, ui, t::TEXT, name, text, DT_LEFT | DT_VCENTER);
                }
                Field::Choice { label: name, options, index } => {
                    gdi::text(dc, ui, t::TEXT, name, label, DT_LEFT | DT_VCENTER);
                    for (k, r) in Self::option_rects(&control, options.len(), m).iter().enumerate() {
                        let on = k == *index;
                        let hovered = self.hover == Some(SheetHit::Option(i, k));
                        let bg = if on { t::rgb(0x2B4F70) } else if hovered { t::rgb(0x3C3C3C) } else { t::KEY_BG };
                        gdi::round_fill(dc, r, m.px(4), bg);
                        gdi::text(dc, small, if on { t::TEXT_BRIGHT } else { t::TEXT }, options[k], *r, DT_CENTER | DT_VCENTER);
                    }
                }
            }
        }
        for (hit, r, label) in [(SheetHit::Cancel, l.cancel, "Cancel"), (SheetHit::Ok, l.ok, self.ok_label)] {
            let hovered = self.hover == Some(hit);
            let bg = match hit {
                SheetHit::Ok => if hovered { t::rgb(0x026EC1) } else { t::ACCENT },
                _ => if hovered { t::rgb(0x3C3C3C) } else { t::KEY_BG },
            };
            gdi::round_fill(dc, &r, m.px(4), bg);
            gdi::text(dc, ui, if hit == SheetHit::Ok { t::TEXT_BRIGHT } else { t::TEXT }, label, r, DT_CENTER | DT_VCENTER);
        }
    }
}

fn frame(dc: HDC, r: &RECT, color: windows::Win32::Foundation::COLORREF) {
    let (w, h) = (r.right - r.left, r.bottom - r.top);
    gdi::fill(dc, &rect(r.left, r.top, w, 1), color);
    gdi::fill(dc, &rect(r.left, r.bottom - 1, w, 1), color);
    gdi::fill(dc, &rect(r.left, r.top, 1, h), color);
    gdi::fill(dc, &rect(r.right - 1, r.top, 1, h), color);
}

/// Parses page lists like "1-3, 7, 10-" (1-based, inclusive, open ends run to
/// the last page) into sorted zero-based indices. "all" or empty: every page.
pub fn parse_pages(text: &str, count: usize) -> Result<Vec<usize>, String> {
    let mut pages: Vec<usize> = parse_groups(&text.replace(';', ","), count)?.into_iter().flatten().collect();
    pages.sort_unstable();
    pages.dedup();
    if pages.is_empty() {
        return Err("No pages given.".into());
    }
    Ok(pages)
}

/// Parses "1-3; 4-10; 11" into groups (one per `;` or `,`), each zero-based.
pub fn parse_groups(text: &str, count: usize) -> Result<Vec<Vec<usize>>, String> {
    let text = text.trim();
    if text.is_empty() || text.eq_ignore_ascii_case("all") {
        return Ok(vec![(0..count).collect()]);
    }
    let number = |s: &str| -> Result<usize, String> {
        let n: usize = s.trim().parse().map_err(|_| format!("\"{}\" isn't a page number.", s.trim()))?;
        if n == 0 || n > count {
            return Err(format!("There's no page {n} (pages are 1–{count})."));
        }
        Ok(n - 1)
    };
    text.split([',', ';'])
        .filter(|part| !part.trim().is_empty())
        .map(|part| match part.split_once('-') {
            Some((a, b)) => {
                let first = if a.trim().is_empty() { 0 } else { number(a)? };
                let last = if b.trim().is_empty() { count - 1 } else { number(b)? };
                Ok((first.min(last)..=first.max(last)).collect())
            }
            None => Ok(vec![number(part)?]),
        })
        .collect()
}

/// "1-3, 5" for zero-based pages.
pub fn format_pages(pages: &[usize]) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < pages.len() {
        let start = pages[i];
        while i + 1 < pages.len() && pages[i + 1] == pages[i] + 1 {
            i += 1;
        }
        let end = pages[i];
        out.push(if end > start { format!("{}-{}", start + 1, end + 1) } else { format!("{}", start + 1) });
        i += 1;
    }
    out.join(", ")
}

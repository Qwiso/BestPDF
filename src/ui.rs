//! The window's contents: menu strip, tab strip, page view, minimap, scrollbar
//! and status bar. Owns all open tabs. Painting is double-buffered GDI.
//!
//! Handlers never block or open modal UI themselves; anything that does
//! (menus, dialogs) is returned as an `Action` for main.rs to perform with no
//! borrow held, since those calls re-enter the window procedure.

use std::path::Path;

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;

use crate::document::Document;
use crate::gdi::{self, rect, BackBuffer, Tint};
use crate::tab::Tab;
use crate::theme::{self as t, Metrics};

pub enum Action {
    None,
    Redraw,
    /// Redraw and start capturing the mouse (a drag began).
    Capture,
    /// Redraw and release the mouse capture (a drag ended).
    Release,
    /// Show the File menu at this client point.
    FileMenu(i32, i32),
}

#[derive(Clone, Copy, PartialEq)]
enum Hit {
    None,
    MenuFile,
    Tab(usize),
    TabClose(usize),
    TabStrip,
    Editor,
    Minimap,
    Scrollbar,
}

#[derive(Clone, Copy, PartialEq)]
enum Drag {
    None,
    /// `grab` = pointer offset from the top of the slider/thumb.
    Minimap { grab: i32 },
    Scrollbar { grab: i32 },
}

struct Regions {
    menu: RECT,
    file_button: RECT,
    tabs: RECT,
    editor: RECT,
    minimap: RECT,
    scrollbar: RECT,
    status: RECT,
}

struct Fonts {
    ui: HFONT,
    status: HFONT,
    badge: HFONT,
    icon: HFONT,
    icon_small: HFONT,
    watermark: HFONT,
}

impl Fonts {
    fn new(dpi: u32) -> Self {
        Self {
            ui: gdi::font("Segoe UI", 13, dpi, 400),
            status: gdi::font("Segoe UI", 12, dpi, 400),
            badge: gdi::font("Segoe UI", 9, dpi, 700),
            icon: gdi::font("Segoe MDL2 Assets", 13, dpi, 400),
            icon_small: gdi::font("Segoe MDL2 Assets", 10, dpi, 400),
            watermark: gdi::font("Segoe UI Light", 40, dpi, 300),
        }
    }

    fn delete(&self) {
        for f in [self.ui, self.status, self.badge, self.icon, self.icon_small, self.watermark] {
            gdi::delete_font(f);
        }
    }
}

/// What a tab shows. A welcome tab (Ctrl+T) is replaced by the next file opened.
enum Slot {
    Welcome,
    Doc(Tab),
}

impl Slot {
    fn name(&self) -> String {
        match self {
            Slot::Welcome => "Welcome".into(),
            Slot::Doc(tab) => tab.name(),
        }
    }
}

/// Minimap geometry for the active tab. `f` maps content px → minimap px.
struct MiniGeo {
    f: f32,
    offset: i32,
    slider: RECT,
    /// Slider px moved per content px scrolled (inverse used while dragging).
    k: f32,
}

pub struct App {
    tabs: Vec<Slot>,
    active: usize,
    m: Metrics,
    fonts: Fonts,
    back: Option<BackBuffer>,
    tint: Tint,
    measure: HDC,
    size: (i32, i32),
    hover: Hit,
    drag: Drag,
    tab_scroll: i32,
    next_untitled: u32,
    /// While the user drags the window edge, cached pages are stretched instead
    /// of re-rendered every frame; a crisp render happens when the drag ends.
    pub live_resize: bool,
}

impl App {
    pub fn new(dpi: u32) -> Self {
        Self {
            tabs: Vec::new(),
            active: 0,
            m: Metrics { dpi },
            fonts: Fonts::new(dpi),
            back: None,
            tint: Tint::new(),
            measure: unsafe { CreateCompatibleDC(None) },
            size: (0, 0),
            hover: Hit::None,
            drag: Drag::None,
            tab_scroll: 0,
            next_untitled: 1,
            live_resize: false,
        }
    }

    pub fn set_dpi(&mut self, dpi: u32) {
        self.fonts.delete();
        self.m = Metrics { dpi };
        self.fonts = Fonts::new(dpi);
    }

    pub fn resize(&mut self, w: i32, h: i32) {
        self.size = (w, h);
        self.clamp_tab_scroll();
    }

    // ---- tabs ------------------------------------------------------------

    pub fn has_tabs(&self) -> bool {
        !self.tabs.is_empty()
    }

    /// The active tab's document view; None if no tabs or it's a welcome tab.
    pub fn active_tab(&self) -> Option<&Tab> {
        match self.tabs.get(self.active) {
            Some(Slot::Doc(tab)) => Some(tab),
            _ => None,
        }
    }

    pub fn active_tab_mut(&mut self) -> Option<&mut Tab> {
        match self.tabs.get_mut(self.active) {
            Some(Slot::Doc(tab)) => Some(tab),
            _ => None,
        }
    }

    /// Activates the tab already showing `path`, if any.
    pub fn focus_path(&mut self, path: &Path) -> bool {
        let shows = |s: &Slot| matches!(s, Slot::Doc(t) if t.doc.path.as_deref() == Some(path));
        match self.tabs.iter().position(shows) {
            Some(i) => {
                self.activate(i);
                true
            }
            None => false,
        }
    }

    pub fn add_document(&mut self, doc: Document) {
        let untitled = doc.path.is_none().then(|| {
            self.next_untitled += 1;
            self.next_untitled - 1
        });
        let slot = Slot::Doc(Tab::new(doc, untitled));
        if let Some(welcome @ Slot::Welcome) = self.tabs.get_mut(self.active) {
            *welcome = slot; // a file opened from a welcome tab takes its place
            self.activate(self.active);
        } else {
            self.insert_tab(slot);
        }
    }

    pub fn add_welcome(&mut self) {
        self.insert_tab(Slot::Welcome);
    }

    /// New tabs open to the right of the active one, like VS Code.
    fn insert_tab(&mut self, slot: Slot) {
        let at = if self.tabs.is_empty() { 0 } else { self.active + 1 };
        self.tabs.insert(at, slot);
        self.activate(at);
    }

    fn activate(&mut self, i: usize) {
        self.active = i.min(self.tabs.len().saturating_sub(1));
        self.reveal_active_tab();
    }

    pub fn close_tab(&mut self, i: usize) {
        if i >= self.tabs.len() {
            return;
        }
        self.tabs.remove(i);
        if self.tabs.is_empty() {
            self.active = 0;
            self.tab_scroll = 0;
            self.hover = Hit::None;
            return;
        }
        if i < self.active || self.active >= self.tabs.len() {
            self.active = self.active.saturating_sub(1);
        }
        self.clamp_tab_scroll();
        self.reveal_active_tab();
    }

    pub fn close_active(&mut self) {
        self.close_tab(self.active);
    }

    pub fn cycle_tab(&mut self, forward: bool) {
        let n = self.tabs.len();
        if n > 1 {
            self.activate(if forward { (self.active + 1) % n } else { (self.active + n - 1) % n });
        }
    }

    /// Client point where the File menu drops down (for Alt+F).
    pub fn file_menu_point(&self) -> (i32, i32) {
        let b = self.regions().file_button;
        (b.left, b.bottom)
    }

    pub fn title(&self) -> String {
        match self.tabs.get(self.active) {
            Some(slot) => format!("{} — Best PDF", slot.name()),
            None => "Best PDF".into(),
        }
    }

    // ---- geometry --------------------------------------------------------

    fn regions(&self) -> Regions {
        let (w, h) = self.size;
        let m = self.m;
        let menu = rect(0, 0, w, m.px(t::MENU_H));
        let label_w = gdi::text_width(self.measure, self.fonts.ui, "File") + m.px(16);
        let file_button = rect(m.px(6), m.px(4), label_w, menu.bottom - m.px(8));
        let tabs_h = if self.has_tabs() { m.px(t::TAB_H) } else { 0 };
        let tabs = rect(0, menu.bottom, w, tabs_h);
        let status = rect(0, h - m.px(t::STATUS_H), w, m.px(t::STATUS_H));
        let body_top = tabs.bottom;
        let body_h = (status.top - body_top).max(0);
        let doc_open = self.active_tab().is_some();
        let (sb_w, mm_w) = if doc_open { (m.px(t::SCROLLBAR_W), m.px(t::MINIMAP_W)) } else { (0, 0) };
        Regions {
            menu,
            file_button,
            tabs,
            editor: rect(0, body_top, (w - sb_w - mm_w).max(0), body_h),
            minimap: rect(w - sb_w - mm_w, body_top, mm_w, body_h),
            scrollbar: rect(w - sb_w, body_top, sb_w, body_h),
            status,
        }
    }

    /// (tab rect, close-button rect) for every tab, in client coordinates.
    fn tab_rects(&self, strip: &RECT) -> Vec<(RECT, RECT)> {
        let m = self.m;
        let mut x = strip.left - self.tab_scroll;
        self.tabs
            .iter()
            .map(|tab| {
                let text_w = gdi::text_width(self.measure, self.fonts.ui, &tab.name());
                let w = (m.px(12 + 22 + 6) + text_w + m.px(8 + 28)).clamp(m.px(90), m.px(260));
                let r = rect(x, strip.top, w, strip.bottom - strip.top);
                let close = rect(r.right - m.px(28), r.top + (r.bottom - r.top - m.px(20)) / 2, m.px(20), m.px(20));
                x += w;
                (r, close)
            })
            .collect()
    }

    fn tabs_width(&self, strip: &RECT) -> i32 {
        self.tab_rects(strip).last().map_or(0, |(r, _)| r.right + self.tab_scroll - strip.left)
    }

    fn clamp_tab_scroll(&mut self) {
        let strip = self.regions().tabs;
        let max = (self.tabs_width(&strip) - (strip.right - strip.left)).max(0);
        self.tab_scroll = self.tab_scroll.clamp(0, max);
    }

    fn reveal_active_tab(&mut self) {
        let strip = self.regions().tabs;
        if let Some(&(r, _)) = self.tab_rects(&strip).get(self.active) {
            if r.left < strip.left {
                self.tab_scroll -= strip.left - r.left;
            } else if r.right > strip.right {
                self.tab_scroll += r.right - strip.right;
            }
        }
        self.clamp_tab_scroll();
    }

    fn mini_geo(&self, tab: &Tab, mm: &RECT, view_h: i32) -> MiniGeo {
        let pad = self.m.px(8);
        let mm_h = mm.bottom - mm.top;
        let f = (mm.right - mm.left - 2 * pad).max(1) as f32 / tab.max_page_w().max(1) as f32;
        let content = (tab.content_h() as f32 * f).round() as i32;
        let max_scroll = tab.max_scroll(view_h);
        let overflow = (content - mm_h).max(0);
        // Long documents: the minimap itself scrolls in proportion (VS Code behavior).
        let offset = if max_scroll > 0 { (tab.scroll_y as i64 * overflow as i64 / max_scroll as i64) as i32 } else { 0 };
        let slider_h = ((view_h as f32 * f).round() as i32).clamp(self.m.px(6), mm_h.max(1));
        let top = mm.top + (tab.scroll_y as f32 * f).round() as i32 - offset;
        let k = if max_scroll > 0 { f - overflow as f32 / max_scroll as f32 } else { f };
        MiniGeo { f, offset, slider: RECT { left: mm.left, top, right: mm.right, bottom: top + slider_h }, k: k.max(1e-4) }
    }

    /// (thumb rect, travel in px) for the scrollbar, or None if nothing to scroll.
    fn scroll_thumb(&self, tab: &Tab, sb: &RECT, view_h: i32) -> Option<(RECT, i32)> {
        let max = tab.max_scroll(view_h);
        if max == 0 {
            return None;
        }
        let track = sb.bottom - sb.top;
        let th = ((track as i64 * view_h as i64 / tab.content_h().max(1) as i64) as i32).clamp(self.m.px(20).min(track), track);
        let travel = track - th;
        let top = sb.top + (tab.scroll_y as i64 * travel as i64 / max as i64) as i32;
        Some((RECT { left: sb.left + self.m.px(3), top, right: sb.right - self.m.px(3), bottom: top + th }, travel))
    }

    fn hit_test(&self, x: i32, y: i32) -> Hit {
        let r = self.regions();
        if gdi::contains(&r.file_button, x, y) {
            return Hit::MenuFile;
        }
        if gdi::contains(&r.tabs, x, y) {
            for (i, (tab, close)) in self.tab_rects(&r.tabs).iter().enumerate() {
                if gdi::contains(close, x, y) {
                    return Hit::TabClose(i);
                }
                if gdi::contains(tab, x, y) {
                    return Hit::Tab(i);
                }
            }
            return Hit::TabStrip;
        }
        if gdi::contains(&r.minimap, x, y) {
            return Hit::Minimap;
        }
        if gdi::contains(&r.scrollbar, x, y) {
            return Hit::Scrollbar;
        }
        if gdi::contains(&r.editor, x, y) {
            return Hit::Editor;
        }
        Hit::None
    }

    // ---- scrolling -------------------------------------------------------

    /// Runs `f` on the active tab with its layout current and the viewport height.
    fn with_view<R>(&mut self, f: impl FnOnce(&mut Tab, i32) -> R) -> Option<R> {
        let editor = self.regions().editor;
        let (m, view_h) = (self.m, editor.bottom - editor.top);
        let tab = self.active_tab_mut()?;
        tab.ensure_layout(editor.right - editor.left, m);
        Some(f(tab, view_h))
    }

    fn scroll_to(&mut self, y: i32) -> Action {
        match self.with_view(|tab, view_h| tab.scroll_to(y, view_h)) {
            Some(true) => Action::Redraw,
            _ => Action::None,
        }
    }

    fn scroll_by(&mut self, dy: i32) -> Action {
        let y = self.active_tab().map_or(0, |t| t.scroll_y);
        self.scroll_to(y.saturating_add(dy))
    }

    fn view_h(&self) -> i32 {
        let e = self.regions().editor;
        e.bottom - e.top
    }

    // ---- input -----------------------------------------------------------

    pub fn key(&mut self, key: VIRTUAL_KEY) -> Action {
        let line = self.m.px(t::LINE_STEP);
        let page = self.view_h() - line;
        match key {
            VK_UP => self.scroll_by(-line),
            VK_DOWN => self.scroll_by(line),
            VK_PRIOR => self.scroll_by(-page),
            VK_NEXT | VK_SPACE => self.scroll_by(page),
            VK_HOME => self.scroll_to(0),
            VK_END => self.scroll_to(i32::MAX),
            _ => Action::None,
        }
    }

    pub fn wheel(&mut self, x: i32, y: i32, delta: i32) -> Action {
        if matches!(self.hit_test(x, y), Hit::TabStrip | Hit::Tab(_) | Hit::TabClose(_)) {
            self.tab_scroll -= delta * self.m.px(40) / 120;
            self.clamp_tab_scroll();
            return Action::Redraw;
        }
        self.scroll_by(-delta * self.m.px(t::LINE_STEP) * 3 / 120)
    }

    pub fn mouse_move(&mut self, x: i32, y: i32) -> Action {
        match self.drag {
            Drag::Minimap { grab } => {
                let r = self.regions();
                let Some(tab) = self.active_tab() else { return Action::None };
                let k = self.mini_geo(tab, &r.minimap, self.view_h()).k;
                let slider_top = y - grab - r.minimap.top;
                return self.scroll_to((slider_top as f32 / k).round() as i32);
            }
            Drag::Scrollbar { grab } => {
                let r = self.regions();
                let view_h = self.view_h();
                let Some(tab) = self.active_tab() else { return Action::None };
                let Some((_, travel)) = self.scroll_thumb(tab, &r.scrollbar, view_h) else { return Action::None };
                let max = tab.max_scroll(view_h);
                let pos = (y - grab - r.scrollbar.top) as i64 * max as i64 / travel.max(1) as i64;
                return self.scroll_to(pos as i32);
            }
            Drag::None => {}
        }
        let hit = self.hit_test(x, y);
        if hit != self.hover {
            self.hover = hit;
            return Action::Redraw;
        }
        Action::None
    }

    pub fn mouse_leave(&mut self) -> Action {
        if self.hover != Hit::None && self.drag == Drag::None {
            self.hover = Hit::None;
            return Action::Redraw;
        }
        Action::None
    }

    pub fn left_down(&mut self, x: i32, y: i32) -> Action {
        let r = self.regions();
        match self.hit_test(x, y) {
            Hit::MenuFile => Action::FileMenu(r.file_button.left, r.file_button.bottom),
            Hit::TabClose(i) => {
                self.close_tab(i);
                self.hover = self.hit_test(x, y);
                Action::Redraw
            }
            Hit::Tab(i) => {
                self.activate(i);
                Action::Redraw
            }
            Hit::Minimap => {
                let view_h = self.view_h();
                let Some(tab) = self.active_tab() else { return Action::None };
                let geo = self.mini_geo(tab, &r.minimap, view_h);
                if !gdi::contains(&geo.slider, x, y) {
                    // Jump so the clicked spot is centered, then keep dragging from there.
                    let content_y = ((y - r.minimap.top + geo.offset) as f32 / geo.f) as i32;
                    self.scroll_to(content_y - view_h / 2);
                }
                let Some(tab) = self.active_tab() else { return Action::None };
                let slider = self.mini_geo(tab, &r.minimap, view_h).slider;
                self.drag = Drag::Minimap { grab: y - slider.top };
                Action::Capture
            }
            Hit::Scrollbar => {
                let view_h = self.view_h();
                let Some(tab) = self.active_tab() else { return Action::None };
                let Some((thumb, _)) = self.scroll_thumb(tab, &r.scrollbar, view_h) else { return Action::None };
                if y < thumb.top {
                    self.scroll_by(-(view_h - self.m.px(t::LINE_STEP)))
                } else if y >= thumb.bottom {
                    self.scroll_by(view_h - self.m.px(t::LINE_STEP))
                } else {
                    self.drag = Drag::Scrollbar { grab: y - thumb.top };
                    Action::Capture
                }
            }
            _ => Action::None,
        }
    }

    pub fn middle_down(&mut self, x: i32, y: i32) -> Action {
        match self.hit_test(x, y) {
            Hit::Tab(i) | Hit::TabClose(i) => {
                self.close_tab(i);
                self.hover = self.hit_test(x, y);
                Action::Redraw
            }
            _ => Action::None,
        }
    }

    pub fn left_up(&mut self) -> Action {
        if self.drag == Drag::None {
            return Action::None;
        }
        self.drag = Drag::None;
        Action::Release
    }

    /// Mouse capture was taken away (alt-tab, etc.).
    pub fn capture_lost(&mut self) {
        self.drag = Drag::None;
    }

    // ---- painting --------------------------------------------------------

    pub fn paint(&mut self, screen: HDC) {
        let (w, h) = self.size;
        if w <= 0 || h <= 0 {
            return;
        }
        let dc = BackBuffer::ensure(&mut self.back, screen, w, h);
        let r = self.regions();
        gdi::fill(dc, &r.editor, t::EDITOR_BG);
        if self.active_tab().is_some() {
            self.paint_pages(dc, &r);
            self.paint_minimap(dc, &r);
            self.paint_scrollbar(dc, &r);
        } else {
            self.paint_watermark(dc, &r.editor);
        }
        if self.has_tabs() {
            self.paint_tabs(dc, &r);
        }
        self.paint_menu(dc, &r);
        self.paint_status(dc, &r);
        unsafe { let _ = BitBlt(screen, 0, 0, w, h, Some(dc), 0, 0, SRCCOPY); }
    }

    fn paint_pages(&mut self, dc: HDC, r: &Regions) {
        let render = !self.live_resize;
        let m = self.m;
        let shadow = m.px(2);
        self.with_view(|tab, view_h| {
            tab.scroll_to(tab.scroll_y, view_h); // re-clamp after resizes
            let top = tab.scroll_y;
            for i in tab.pages_in(top, top + view_h) {
                let p = tab.pages()[i];
                let y = r.editor.top + p.y - top;
                gdi::fill(dc, &rect(p.x + shadow, y + shadow, p.w, p.h), t::PAGE_SHADOW);
                match tab.page(i, p.w, render) {
                    Some(bmp) => gdi::blit(dc, bmp, p.x, y, p.w, p.h),
                    None => gdi::fill(dc, &rect(p.x, y, p.w, p.h), t::WHITE),
                }
            }
        });
    }

    fn paint_minimap(&mut self, dc: HDC, r: &Regions) {
        let mm = r.minimap;
        let view_h = r.editor.bottom - r.editor.top;
        let alpha = match (self.drag, self.hover) {
            (Drag::Minimap { .. }, _) => t::SLIDER_ALPHA_ACTIVE,
            (_, Hit::Minimap) => t::SLIDER_ALPHA_HOVER,
            _ => t::SLIDER_ALPHA,
        };
        let Some(tab) = self.active_tab() else { return };
        let geo = self.mini_geo(tab, &mm, view_h);
        gdi::fill(dc, &mm, t::EDITOR_BG);

        let Some(tab) = self.active_tab_mut() else { return };
        let top = (geo.offset as f32 / geo.f) as i32;
        let bottom = ((geo.offset + mm.bottom - mm.top) as f32 / geo.f) as i32 + 1;
        let visible = tab.pages_in(top, bottom);
        for i in visible.clone() {
            let p = tab.pages()[i];
            let (tw, th) = (((p.w as f32) * geo.f).round().max(1.0) as i32, ((p.h as f32) * geo.f).round().max(1.0) as i32);
            let x = mm.left + (mm.right - mm.left - tw) / 2;
            let y = mm.top + (p.y as f32 * geo.f).round() as i32 - geo.offset;
            match tab.thumb(i, tw, true) {
                Some(bmp) => gdi::blit(dc, bmp, x, y, tw, th),
                None => gdi::fill(dc, &rect(x, y, tw, th), t::WHITE),
            }
        }
        let pages = tab.pages_in(tab.scroll_y, tab.scroll_y + view_h);
        tab.evict(pages, visible); // both caches, now that both views are drawn
        self.tint.fill(dc, &geo.slider, t::SLIDER, alpha);
    }

    fn paint_scrollbar(&self, dc: HDC, r: &Regions) {
        gdi::fill(dc, &r.scrollbar, t::EDITOR_BG);
        gdi::fill(dc, &rect(r.scrollbar.left, r.scrollbar.top, 1, r.scrollbar.bottom - r.scrollbar.top), t::BORDER);
        let view_h = r.editor.bottom - r.editor.top;
        let Some(tab) = self.active_tab() else { return };
        if let Some((thumb, _)) = self.scroll_thumb(tab, &r.scrollbar, view_h) {
            let alpha = match (self.drag, self.hover) {
                (Drag::Scrollbar { .. }, _) => t::SLIDER_ALPHA_ACTIVE,
                (_, Hit::Scrollbar) => t::SLIDER_ALPHA_HOVER,
                _ => t::SLIDER_ALPHA_HOVER - 0x10,
            };
            self.tint.fill(dc, &thumb, t::SLIDER, alpha);
        }
    }

    fn paint_tabs(&self, dc: HDC, r: &Regions) {
        let m = self.m;
        let strip = r.tabs;
        gdi::fill(dc, &strip, t::CHROME_BG);
        gdi::fill(dc, &rect(strip.left, strip.bottom - 1, strip.right - strip.left, 1), t::BORDER);
        for (i, (tr, close)) in self.tab_rects(&strip).into_iter().enumerate() {
            if tr.right < strip.left || tr.left > strip.right {
                continue;
            }
            let active = i == self.active;
            let hovered = matches!(self.hover, Hit::Tab(h) | Hit::TabClose(h) if h == i);
            if active || hovered {
                gdi::fill(dc, &rect(tr.left, tr.top, tr.right - tr.left, tr.bottom - tr.top), t::EDITOR_BG);
            }
            if active {
                gdi::fill(dc, &rect(tr.left, tr.top, tr.right - tr.left, 1), t::ACCENT);
            }
            gdi::fill(dc, &rect(tr.right - 1, tr.top, 1, tr.bottom - tr.top), t::BORDER);

            let icon = rect(tr.left + m.px(12), tr.top, m.px(22), tr.bottom - tr.top);
            match self.tabs[i] {
                Slot::Doc(_) => gdi::text(dc, self.fonts.badge, t::PDF_ICON, "PDF", icon, DT_LEFT | DT_VCENTER),
                Slot::Welcome => gdi::text(dc, self.fonts.icon, t::ACCENT, t::ICON_HOME, icon, DT_LEFT | DT_VCENTER),
            }
            let label = RECT { left: icon.right + m.px(6), right: close.left - m.px(4), ..tr };
            let color = if active { t::TEXT_BRIGHT } else { t::TEXT_DIM };
            gdi::text(dc, self.fonts.ui, color, &self.tabs[i].name(), label, DT_LEFT | DT_VCENTER | DT_END_ELLIPSIS);

            if active || hovered {
                if self.hover == Hit::TabClose(i) {
                    gdi::round_fill(dc, &close, m.px(5), t::HOVER_BG);
                }
                gdi::text(dc, self.fonts.icon_small, color, t::ICON_CLOSE, close, DT_CENTER | DT_VCENTER);
            }
        }
    }

    fn paint_menu(&self, dc: HDC, r: &Regions) {
        gdi::fill(dc, &r.menu, t::CHROME_BG);
        gdi::fill(dc, &rect(0, r.menu.bottom - 1, r.menu.right, 1), t::BORDER);
        if self.hover == Hit::MenuFile {
            gdi::round_fill(dc, &r.file_button, self.m.px(5), t::HOVER_BG);
        }
        gdi::text(dc, self.fonts.ui, t::TEXT, "File", r.file_button, DT_CENTER | DT_VCENTER);
    }

    fn paint_status(&self, dc: HDC, r: &Regions) {
        let s = r.status;
        gdi::fill(dc, &s, t::CHROME_BG);
        gdi::fill(dc, &rect(0, s.top, s.right, 1), t::BORDER);
        let Some(tab) = self.active_tab() else { return };
        let pad = self.m.px(12);
        let view_h = r.editor.bottom - r.editor.top;
        let current = tab.current_page(view_h);
        let left = format!("Page {} of {}", current + 1, tab.doc.page_count());
        gdi::text(dc, self.fonts.status, t::TEXT, &left, RECT { left: pad, ..s }, DT_LEFT | DT_VCENTER);
        let (pw, ph) = tab.doc.page_sizes.get(current).copied().unwrap_or_default();
        let right = format!("{:.2} × {:.2} in      PDF", pw / 72.0, ph / 72.0);
        gdi::text(dc, self.fonts.status, t::TEXT, &right, RECT { right: s.right - pad, ..s }, DT_RIGHT | DT_VCENTER);
    }

    fn paint_watermark(&self, dc: HDC, e: &RECT) {
        let m = self.m;
        let cx = (e.left + e.right) / 2;
        let cy = e.top + (e.bottom - e.top) * 2 / 5;
        gdi::text(dc, self.fonts.watermark, t::TEXT_FAINT, "Best PDF", rect(e.left, cy - m.px(80), e.right - e.left, m.px(60)), DT_CENTER | DT_VCENTER);
        let rows = [
            ("Open File", "Ctrl+O"),
            ("New Tab", "Ctrl+T"),
            ("New Document", "Ctrl+N"),
            ("Close Tab", "Ctrl+W"),
            ("Switch Tab", "Ctrl+Tab"),
        ];
        for (n, (label, keys)) in rows.iter().enumerate() {
            let y = cy + n as i32 * m.px(30);
            let row_h = m.px(24);
            gdi::text(dc, self.fonts.ui, t::TEXT_DIM, label, RECT { left: e.left, top: y, right: cx - m.px(10), bottom: y + row_h }, DT_RIGHT | DT_VCENTER);
            let mut x = cx + m.px(10);
            for (k, key) in keys.split('+').enumerate() {
                if k > 0 {
                    gdi::text(dc, self.fonts.ui, t::TEXT_FAINT, "+", rect(x, y, m.px(14), row_h), DT_CENTER | DT_VCENTER);
                    x += m.px(14);
                }
                let kw = gdi::text_width(self.measure, self.fonts.status, key) + m.px(12);
                let key_rect = rect(x, y + m.px(2), kw, row_h - m.px(4));
                gdi::round_fill(dc, &key_rect, m.px(6), t::KEY_BG);
                gdi::text(dc, self.fonts.status, t::TEXT, key, key_rect, DT_CENTER | DT_VCENTER);
                x += kw;
            }
        }
        let hint = rect(e.left, cy + rows.len() as i32 * m.px(30) + m.px(12), e.right - e.left, m.px(24));
        gdi::text(dc, self.fonts.status, t::TEXT_FAINT, "or drop PDF files anywhere in this window", hint, DT_CENTER | DT_VCENTER);
    }
}

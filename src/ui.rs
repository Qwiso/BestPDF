//! The window's contents: menu strip, tab strip, page view, minimap, scrollbar
//! and status bar. Owns all open tabs. Painting is double-buffered GDI.
//!
//! Handlers never block or open modal UI themselves; anything that does
//! (menus, dialogs) is returned as an `Action` for main.rs to perform with no
//! borrow held, since those calls re-enter the window procedure.

use std::path::Path;
use std::time::{Duration, Instant};

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
    /// Redraw and release the mouse capture (a drag ended), reporting any error.
    Release(Option<String>),
    /// Show the File menu at this client point.
    FileMenu(i32, i32),
    /// Close this tab, asking first if it has unsaved changes.
    CloseTab(usize),
}

/// How long the pointer must rest on a tab during a page drag to switch to it.
const SPRING_LOAD: Duration = Duration::from_millis(450);

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
    /// Pressed on a minimap page; a click jumps there, moving the pointer
    /// starts a page drag.
    PagePress { page: usize, x: i32, y: i32 },
    Page(PageDrag),
}

#[derive(Clone, Copy, PartialEq)]
struct PageDrag {
    /// Source tab and page.
    from: usize,
    page: usize,
    /// Pointer, client coordinates.
    x: i32,
    y: i32,
    /// Ctrl held: copy instead of move.
    copy: bool,
    target: DropTarget,
    /// Tab under the pointer and since when (switches to it after SPRING_LOAD).
    hover_tab: Option<(usize, Instant)>,
}

#[derive(Clone, Copy, PartialEq)]
enum DropTarget {
    Nothing,
    /// Insert before this page index in the active tab (== page count: at end).
    Minimap(usize),
    /// Append to this tab's document (a welcome tab becomes a new document).
    Tab(usize),
    /// Empty tab strip: a new document tab.
    NewTab,
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

fn ctrl_down() -> bool {
    unsafe { GetKeyState(VK_CONTROL.0 as i32) < 0 }
}

/// A move may not leave the source document with no pages.
fn check_can_take(src: &Tab, d: PageDrag) -> Result<(), String> {
    if !d.copy && src.doc.page_count() <= 1 {
        return Err(format!(
            "{} has only one page, so it can't be moved out. Hold Ctrl while dropping to copy it instead.",
            src.name()
        ));
    }
    Ok(())
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

    fn make_tab(&mut self, doc: Document) -> Tab {
        let untitled = doc.path.is_none().then(|| {
            self.next_untitled += 1;
            self.next_untitled - 1
        });
        Tab::new(doc, untitled)
    }

    pub fn add_document(&mut self, doc: Document) {
        let slot = Slot::Doc(self.make_tab(doc));
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

    pub fn activate(&mut self, i: usize) {
        self.active = i.min(self.tabs.len().saturating_sub(1));
        self.reveal_active_tab();
    }

    /// Name of tab `i` if it has unsaved changes.
    pub fn unsaved_name(&self, i: usize) -> Option<String> {
        match self.tabs.get(i) {
            Some(Slot::Doc(tab)) if tab.dirty => Some(tab.name()),
            _ => None,
        }
    }

    pub fn first_unsaved(&self) -> Option<usize> {
        (0..self.tabs.len()).find(|&i| self.unsaved_name(i).is_some())
    }

    pub fn close_tab(&mut self, i: usize) {
        if i >= self.tabs.len() {
            return;
        }
        // Tab indices shift; a page drag in progress would point at the wrong tab.
        if matches!(self.drag, Drag::Page(_) | Drag::PagePress { .. }) {
            self.drag = Drag::None;
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

    pub fn active_index(&self) -> usize {
        self.active
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
            Some(Slot::Doc(tab)) if tab.dirty => format!("● {} — Best PDF", tab.name()),
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

    /// Brings the active tab's layout up to date. Layout is otherwise rebuilt
    /// at paint time, and input can arrive between a page edit and the repaint.
    fn sync_layout(&mut self) {
        self.with_view(|_, _| ());
    }

    fn view_h(&self) -> i32 {
        let e = self.regions().editor;
        e.bottom - e.top
    }

    // ---- pages in the minimap -------------------------------------------

    /// Minimap rect of page `i` of `tab`.
    fn thumb_rect(&self, tab: &Tab, geo: &MiniGeo, mm: &RECT, i: usize) -> RECT {
        let p = tab.pages()[i];
        let w = ((p.w as f32) * geo.f).round().max(1.0) as i32;
        let h = ((p.h as f32) * geo.f).round().max(1.0) as i32;
        let x = mm.left + (mm.right - mm.left - w) / 2;
        let y = mm.top + (p.y as f32 * geo.f).round() as i32 - geo.offset;
        rect(x, y, w, h)
    }

    /// Pages of `tab` that show in the minimap.
    fn minimap_pages(&self, tab: &Tab, geo: &MiniGeo, mm: &RECT) -> std::ops::Range<usize> {
        let top = (geo.offset as f32 / geo.f) as i32;
        let bottom = ((geo.offset + mm.bottom - mm.top) as f32 / geo.f) as i32 + 1;
        tab.pages_in(top, bottom)
    }

    /// Page of the active tab under a minimap point.
    fn page_at(&self, x: i32, y: i32) -> Option<usize> {
        let r = self.regions();
        let tab = self.active_tab()?;
        let geo = self.mini_geo(tab, &r.minimap, self.view_h());
        self.minimap_pages(tab, &geo, &r.minimap)
            .find(|&i| gdi::contains(&self.thumb_rect(tab, &geo, &r.minimap, i), x, y))
    }

    /// Where a page dropped at minimap height `y` goes: before the first page
    /// whose middle is below the pointer.
    fn insert_index_at(&self, y: i32) -> Option<usize> {
        let r = self.regions();
        let tab = self.active_tab()?;
        let geo = self.mini_geo(tab, &r.minimap, self.view_h());
        let content_y = ((y - r.minimap.top + geo.offset) as f32 / geo.f) as i32;
        Some(tab.pages().iter().take_while(|p| p.y + p.h / 2 < content_y).count())
    }

    fn drop_target(&self, x: i32, y: i32) -> DropTarget {
        match self.hit_test(x, y) {
            Hit::Minimap => self.insert_index_at(y).map_or(DropTarget::Nothing, DropTarget::Minimap),
            Hit::Tab(i) | Hit::TabClose(i) => DropTarget::Tab(i),
            Hit::TabStrip => DropTarget::NewTab,
            Hit::Editor if self.active_tab().is_none() => DropTarget::Tab(self.active), // welcome screen
            _ => DropTarget::Nothing,
        }
    }

    fn update_page_drag(&self, d: &mut PageDrag, x: i32, y: i32) {
        d.x = x;
        d.y = y;
        d.copy = ctrl_down();
        d.target = self.drop_target(x, y);
        d.hover_tab = match d.target {
            DropTarget::Tab(i) if i != self.active => match d.hover_tab {
                Some((h, since)) if h == i => Some((h, since)),
                _ => Some((i, Instant::now())),
            },
            _ => None,
        };
    }

    // ---- page drag & drop ------------------------------------------------

    fn doc_tab_mut(&mut self, i: usize) -> Option<&mut Tab> {
        match self.tabs.get_mut(i) {
            Some(Slot::Doc(tab)) => Some(tab),
            _ => None,
        }
    }

    /// Two different document tabs, mutably, in the order asked.
    fn two_doc_tabs(&mut self, a: usize, b: usize) -> Option<(&mut Tab, &mut Tab)> {
        if a == b || a.max(b) >= self.tabs.len() {
            return None;
        }
        let (low, high) = self.tabs.split_at_mut(a.max(b));
        let (first, last) = (&mut low[a.min(b)], &mut high[0]);
        let (x, y) = if a < b { (first, last) } else { (last, first) };
        match (x, y) {
            (Slot::Doc(x), Slot::Doc(y)) => Some((x, y)),
            _ => None,
        }
    }

    fn drop_page(&mut self, d: PageDrag) -> Result<(), String> {
        match d.target {
            DropTarget::Nothing => Ok(()),
            DropTarget::Minimap(at) => self.transfer(d, self.active, at),
            DropTarget::Tab(i) => match self.tabs.get(i) {
                Some(Slot::Doc(tab)) => {
                    let at = tab.doc.page_count();
                    self.transfer(d, i, at)?;
                    self.activate(i);
                    Ok(())
                }
                Some(Slot::Welcome) => self.extract(d, Some(i)),
                None => Ok(()),
            },
            DropTarget::NewTab => self.extract(d, None),
        }
    }

    /// Moves (Ctrl: copies) the dragged page into tab `to`, before index `at`.
    fn transfer(&mut self, d: PageDrag, to: usize, at: usize) -> Result<(), String> {
        if d.from == to {
            let Some(tab) = self.doc_tab_mut(to) else { return Ok(()) };
            if d.copy {
                tab.doc.duplicate_page(d.page, at)?;
            } else {
                // `at` counts the page itself, so past it the final index is one less.
                let dest = if at > d.page { at - 1 } else { at };
                if dest == d.page {
                    return Ok(());
                }
                tab.doc.move_page(d.page, dest)?;
            }
            tab.pages_changed();
            return Ok(());
        }
        let Some((src, dst)) = self.two_doc_tabs(d.from, to) else { return Ok(()) };
        check_can_take(src, d)?;
        dst.doc.import_page(&src.doc, d.page, at)?;
        dst.pages_changed();
        if !d.copy {
            src.doc.delete_page(d.page);
            src.pages_changed();
        }
        Ok(())
    }

    /// Puts the dragged page in a new untitled document: into welcome tab
    /// `slot`, or a new tab at the end.
    fn extract(&mut self, d: PageDrag, slot: Option<usize>) -> Result<(), String> {
        let Some(src) = self.doc_tab_mut(d.from) else { return Ok(()) };
        check_can_take(src, d)?;
        let doc = Document::from_page(&src.doc, d.page)?;
        if !d.copy {
            src.doc.delete_page(d.page);
            src.pages_changed();
        }
        let mut tab = self.make_tab(doc);
        tab.dirty = true;
        match slot {
            Some(i) => {
                self.tabs[i] = Slot::Doc(tab);
                self.activate(i);
            }
            None => {
                self.tabs.push(Slot::Doc(tab));
                self.activate(self.tabs.len() - 1);
            }
        }
        Ok(())
    }

    // ---- input -----------------------------------------------------------

    pub fn key(&mut self, key: VIRTUAL_KEY) -> Action {
        if key == VK_ESCAPE && matches!(self.drag, Drag::Page(_) | Drag::PagePress { .. }) {
            self.drag = Drag::None;
            return Action::Release(None);
        }
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
        self.sync_layout();
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
            Drag::PagePress { page, x: x0, y: y0 } => {
                if (x - x0).abs().max((y - y0).abs()) < self.m.px(5) {
                    return Action::None;
                }
                let mut d = PageDrag {
                    from: self.active,
                    page,
                    x,
                    y,
                    copy: false,
                    target: DropTarget::Nothing,
                    hover_tab: None,
                };
                self.update_page_drag(&mut d, x, y);
                self.drag = Drag::Page(d);
                return Action::Redraw;
            }
            Drag::Page(mut d) => {
                self.update_page_drag(&mut d, x, y);
                self.drag = Drag::Page(d);
                return Action::Redraw;
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
        self.sync_layout();
        let r = self.regions();
        match self.hit_test(x, y) {
            Hit::MenuFile => Action::FileMenu(r.file_button.left, r.file_button.bottom),
            Hit::TabClose(i) => Action::CloseTab(i),
            Hit::Tab(i) => {
                self.activate(i);
                Action::Redraw
            }
            Hit::Minimap => {
                let view_h = self.view_h();
                let Some(tab) = self.active_tab() else { return Action::None };
                let geo = self.mini_geo(tab, &r.minimap, view_h);
                let on_slider = gdi::contains(&geo.slider, x, y);
                // A page outside the slider: click to jump, drag to rearrange.
                if !on_slider {
                    if let Some(page) = self.page_at(x, y) {
                        self.drag = Drag::PagePress { page, x, y };
                        return Action::Capture;
                    }
                }
                // On the slider or between pages: scrub like VS Code's minimap.
                if !on_slider {
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
            Hit::Tab(i) | Hit::TabClose(i) => Action::CloseTab(i),
            _ => Action::None,
        }
    }

    /// After a tab closes, re-evaluate what's under the pointer.
    pub fn rehover(&mut self, x: i32, y: i32) {
        self.hover = self.hit_test(x, y);
    }

    pub fn left_up(&mut self) -> Action {
        self.sync_layout();
        match std::mem::replace(&mut self.drag, Drag::None) {
            Drag::None => Action::None,
            Drag::PagePress { page, .. } => {
                // A click on a page: bring it to the top of the view.
                let margin = self.m.px(t::PAGE_MARGIN);
                if let Some(y) = self.active_tab().and_then(|tab| tab.pages().get(page)).map(|p| p.y) {
                    self.scroll_to(y - margin);
                }
                Action::Release(None)
            }
            Drag::Page(d) => Action::Release(self.drop_page(d).err()),
            Drag::Minimap { .. } | Drag::Scrollbar { .. } => Action::Release(None),
        }
    }

    /// Runs on a timer while the mouse is captured: switches to a tab the
    /// dragged page rests on, auto-scrolls near the minimap's ends, and
    /// picks up Ctrl (copy) being pressed or released.
    pub fn tick(&mut self) -> Action {
        let Drag::Page(mut d) = self.drag else { return Action::None };
        let before = d;
        let mut moved = false;
        if let Some((i, since)) = d.hover_tab {
            if since.elapsed() >= SPRING_LOAD && i != self.active {
                self.activate(i);
                self.sync_layout();
                moved = true;
            }
        }
        let mm = self.regions().minimap;
        if d.x >= mm.left && d.x < mm.right && d.y >= mm.top && d.y < mm.bottom {
            let zone = self.m.px(40);
            let dy = if d.y < mm.top + zone {
                d.y - (mm.top + zone)
            } else if d.y >= mm.bottom - zone {
                d.y - (mm.bottom - zone)
            } else {
                0
            };
            if dy != 0 && matches!(self.scroll_by(dy * 3), Action::Redraw) {
                moved = true;
            }
        }
        let (x, y) = (d.x, d.y);
        self.update_page_drag(&mut d, x, y);
        self.drag = Drag::Page(d);
        if moved || d.copy != before.copy || d.target != before.target {
            Action::Redraw
        } else {
            Action::None
        }
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
        self.paint_drag_ghost(dc);
        unsafe { let _ = BitBlt(screen, 0, 0, w, h, Some(dc), 0, 0, SRCCOPY); }
    }

    /// The dragged page, following the pointer.
    fn paint_drag_ghost(&self, dc: HDC) {
        let Drag::Page(d) = self.drag else { return };
        let Some(Slot::Doc(src)) = self.tabs.get(d.from) else { return };
        let m = self.m;
        let (pw, ph) = src.doc.page_sizes.get(d.page).copied().unwrap_or((612.0, 792.0));
        let w = m.px(72);
        let h = (w as f32 * ph / pw) as i32;
        // Below-right of the pointer, flipped left/up where it would leave the window.
        let (gap, (cw, ch)) = (m.px(14), self.size);
        let x = if d.x + gap + w > cw { d.x - gap - w } else { d.x + gap };
        let y = if d.y + gap + h > ch { d.y - gap - h } else { d.y + gap };
        let border = rect(x - 2, y - 2, w + 4, h + 4);
        gdi::fill(dc, &border, if d.target == DropTarget::Nothing { t::TEXT_FAINT } else { t::ACCENT });
        match src.cached_thumb(d.page) {
            Some(bmp) => gdi::blit(dc, bmp, x, y, w, h),
            None => gdi::fill(dc, &rect(x, y, w, h), t::WHITE),
        }
        if d.copy {
            let badge = rect(x - m.px(9), y - m.px(9), m.px(18), m.px(18));
            gdi::round_fill(dc, &badge, m.px(18), t::ACCENT);
            gdi::text(dc, self.fonts.ui, t::TEXT_BRIGHT, "+", badge, DT_CENTER | DT_VCENTER);
        }
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
        let visible = self.minimap_pages(tab, &geo, &mm);
        let rects: Vec<RECT> = visible.clone().map(|i| self.thumb_rect(tab, &geo, &mm, i)).collect();
        // During a page drag: dim the page being moved, and mark where it would land.
        let (mut dim, mut marker) = (None, None);
        if let Drag::Page(d) = self.drag {
            let here = d.from == self.active;
            if here && !d.copy {
                dim = visible.clone().position(|i| i == d.page).map(|k| rects[k]);
            }
            if let DropTarget::Minimap(at) = d.target {
                let no_op = here && !d.copy && (at == d.page || at == d.page + 1);
                if !no_op {
                    // Midway between page `at - 1` and page `at`.
                    let pages = tab.pages();
                    let half_gap = self.m.px(t::PAGE_GAP) / 2;
                    let content_y = match (at.checked_sub(1).and_then(|i| pages.get(i)), pages.get(at)) {
                        (Some(a), Some(b)) => (a.y + a.h + b.y) / 2,
                        (Some(a), None) => a.y + a.h + half_gap,
                        (None, Some(b)) => b.y - half_gap,
                        (None, None) => 0,
                    };
                    let y = mm.top + (content_y as f32 * geo.f).round() as i32 - geo.offset;
                    let pad = self.m.px(6);
                    marker = Some(rect(mm.left + pad, y - self.m.px(2), mm.right - mm.left - 2 * pad, self.m.px(3)));
                }
            }
        }
        let dragging = matches!(self.drag, Drag::Page(_));

        let Some(tab) = self.active_tab_mut() else { return };
        for (i, r) in visible.clone().zip(&rects) {
            match tab.thumb(i, r.right - r.left, true) {
                Some(bmp) => gdi::blit(dc, bmp, r.left, r.top, r.right - r.left, r.bottom - r.top),
                None => gdi::fill(dc, r, t::WHITE),
            }
        }
        let pages = tab.pages_in(tab.scroll_y, tab.scroll_y + view_h);
        tab.evict(pages, visible); // both caches, now that both views are drawn

        if let Some(r) = dim {
            self.tint.fill(dc, &r, t::EDITOR_BG, 0xB0);
        }
        if let Some(r) = marker {
            gdi::fill(dc, &r, t::ACCENT);
        }
        if !dragging {
            self.tint.fill(dc, &geo.slider, t::SLIDER, alpha);
        }
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

            // VS Code: unsaved tabs show ● where the × goes, until hovered.
            let dirty = matches!(&self.tabs[i], Slot::Doc(tab) if tab.dirty);
            if self.hover == Hit::TabClose(i) {
                gdi::round_fill(dc, &close, m.px(5), t::HOVER_BG);
            }
            if dirty && self.hover != Hit::TabClose(i) {
                let c = m.px(8);
                let dot = rect((close.left + close.right - c) / 2, (close.top + close.bottom - c) / 2, c, c);
                gdi::round_fill(dc, &dot, c, color);
            } else if active || hovered {
                gdi::text(dc, self.fonts.icon_small, color, t::ICON_CLOSE, close, DT_CENTER | DT_VCENTER);
            }

            if matches!(self.drag, Drag::Page(d) if d.target == DropTarget::Tab(i)) {
                self.outline(dc, &tr, t::ACCENT);
            }
        }

        // Empty strip right of the tabs: drop a page here for a new document.
        if let Drag::Page(d) = self.drag {
            let end = self.tab_rects(&strip).last().map_or(strip.left, |(r, _)| r.right);
            if end < strip.right {
                let area = RECT { left: end, ..strip };
                let on = d.target == DropTarget::NewTab;
                if on {
                    gdi::fill(dc, &area, t::HOVER_BG);
                    self.outline(dc, &area, t::ACCENT);
                }
                let label = RECT { left: end + m.px(12), ..strip };
                gdi::text(dc, self.fonts.ui, if on { t::TEXT } else { t::TEXT_FAINT }, "+ New document", label, DT_LEFT | DT_VCENTER);
            }
        }
    }

    fn outline(&self, dc: HDC, r: &RECT, color: windows::Win32::Foundation::COLORREF) {
        let (w, h) = (r.right - r.left, r.bottom - r.top);
        let s = self.m.px(2);
        gdi::fill(dc, &rect(r.left, r.top, w, s), color);
        gdi::fill(dc, &rect(r.left, r.bottom - s, w, s), color);
        gdi::fill(dc, &rect(r.left, r.top, s, h), color);
        gdi::fill(dc, &rect(r.right - s, r.top, s, h), color);
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
        let pad = self.m.px(12);
        if let Drag::Page(d) = self.drag {
            let verb = if d.copy { "Copying" } else { "Moving" };
            let hint = format!(
                "{verb} page {}  ·  drop in the page strip, on a tab, or on the empty tab bar  ·  Ctrl: copy  ·  Esc: cancel",
                d.page + 1
            );
            gdi::text(dc, self.fonts.status, t::TEXT, &hint, RECT { left: pad, ..s }, DT_LEFT | DT_VCENTER | DT_END_ELLIPSIS);
            return;
        }
        let Some(tab) = self.active_tab() else { return };
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

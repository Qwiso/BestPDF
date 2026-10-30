//! The window's contents: menu strip, tab strip, page view, minimap, scrollbar
//! and status bar. Owns all open tabs. Painting is double-buffered GDI.
//!
//! Handlers never block or open modal UI themselves; anything that does
//! (menus, dialogs) is returned as an `Action` for main.rs to perform with no
//! borrow held, since those calls re-enter the window procedure.

use std::path::Path;
use std::rc::Rc;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;

use crate::document::Document;
use crate::gdi::{self, rect, BackBuffer, Tint};
use crate::overlay::{self, Image, Kind, Overlay};
use crate::signature::{Library, SigKind};
use crate::signpad::{PadEvent, SignPad};
use crate::tab::{Tab, ZOOM_DEFAULT, ZOOM_MAX, ZOOM_MIN};
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
    /// Pick a picture for the signature being made.
    ImportSignature,
    /// Ask, then delete this signature from the library.
    DeleteSignature(usize),
    Error(String),
}

/// Mouse pointer shapes the app asks for.
pub enum Cursor {
    Arrow,
    Hand,
    Move,
    SizeNWSE,
    SizeNESW,
    SizeNS,
    SizeWE,
    IBeam,
    Cross,
}

/// How long the pointer must rest on a tab during a page drag to switch to it.
const SPRING_LOAD: Duration = Duration::from_millis(450);
/// Font size of new text boxes, in points.
const TEXT_SIZE: f32 = 14.0;

/// Tool buttons in the menu strip.
#[derive(Clone, Copy, PartialEq)]
enum ToolButton {
    Select,
    Text,
    Highlight,
    Sign,
}

const TOOL_BUTTONS: [(ToolButton, &str); 4] =
    [(ToolButton::Select, "Select"), (ToolButton::Text, "Text"), (ToolButton::Highlight, "Highlight"), (ToolButton::Sign, "Sign ▾")];

/// What a click on empty page space does. Overlays can be moved and resized
/// with any tool.
#[derive(Clone, Copy, PartialEq)]
enum Tool {
    Select,
    Text,
    Highlight,
    /// Stamp this library entry.
    Stamp(usize),
}

/// Entries in the Sign menu.
#[derive(Clone, Copy, PartialEq)]
enum MenuItem {
    Use(usize),
    Delete(usize),
    New(SigKind),
}

struct MenuLayout {
    panel: RECT,
    headers: Vec<(&'static str, RECT)>,
    /// Delete buttons come before their tiles, so they win hit-tests.
    entries: Vec<(MenuItem, RECT)>,
}

enum OverlayHit {
    /// A resize grip: -1/1 = left/right (top/bottom) edge, 0 = neither.
    Grip(usize, i8, i8),
    Body(usize),
}

/// Resize grips around an overlay's screen rect: corners, plus edge middles
/// for overlays that stretch freely.
fn grips(r: &RECT, all_sides: bool, m: Metrics) -> Vec<(i8, i8, RECT)> {
    let g = m.px(8);
    let (cx, cy) = ((r.left + r.right) / 2, (r.top + r.bottom) / 2);
    let mut out = Vec::new();
    for gy in -1i8..=1 {
        for gx in -1i8..=1 {
            let corner = gx != 0 && gy != 0;
            if (gx, gy) == (0, 0) || (!corner && !all_sides) {
                continue;
            }
            let x = [r.left, cx, r.right][(gx + 1) as usize];
            let y = [r.top, cy, r.bottom][(gy + 1) as usize];
            out.push((gx, gy, rect(x - g / 2, y - g / 2, g, g)));
        }
    }
    out
}

/// A box [x, y, w, h] dragged by grip (gx, gy) by (dx, dy). With `aspect`,
/// corner drags scale proportionally around the opposite corner.
fn resize_box([x, y, w, h]: [f32; 4], gx: i8, gy: i8, dx: f32, dy: f32, aspect: bool) -> [f32; 4] {
    let min = overlay::MIN_SIDE;
    let (mut l, mut t, mut r, mut b) = (x, y, x + w, y + h);
    if gx < 0 {
        l = (l + dx).min(r - min);
    } else if gx > 0 {
        r = (r + dx).max(l + min);
    }
    if gy < 0 {
        t = (t + dy).min(b - min);
    } else if gy > 0 {
        b = (b + dy).max(t + min);
    }
    if aspect && gx != 0 && gy != 0 {
        let (sx, sy) = ((r - l) / w, (b - t) / h);
        let k = if (sx - 1.0).abs() > (sy - 1.0).abs() { sx } else { sy };
        let k = k.max(min / w.min(h));
        if gx < 0 {
            l = r - w * k;
        } else {
            r = l + w * k;
        }
        if gy < 0 {
            t = b - h * k;
        } else {
            b = t + h * k;
        }
    }
    [l, t, r - l, b - t]
}

#[derive(Clone, Copy, PartialEq)]
enum Hit {
    None,
    MenuFile,
    Tab(usize),
    TabClose(usize),
    TabStrip,
    Tool(ToolButton),
    Menu(MenuItem),
    /// Inside the Sign menu, but not on an entry.
    MenuPanel,
    Editor,
    Minimap,
    Scrollbar,
    /// The horizontal scrollbar along the bottom of the page view.
    HScroll,
    Zoom(ZoomButton),
}

/// Zoom controls in the status bar.
#[derive(Clone, Copy, PartialEq)]
enum ZoomButton {
    Out,
    /// The percentage: click to go back to the default.
    Reset,
    In,
}

/// Zoom steps for the keyboard and the status bar buttons.
const ZOOM_STEPS: [f32; 12] = [0.10, 0.25, 0.33, 0.50, 0.60, 0.75, 0.90, 1.00, 1.25, 1.50, 2.00, 2.50];

#[derive(Clone, Copy, PartialEq)]
enum Drag {
    None,
    /// `grab` = pointer offset from the top (left) of the thumb.
    Scrollbar { grab: i32 },
    HScroll { grab: i32 },
    /// Pressed on a minimap page; a click jumps there, moving the pointer
    /// starts a page drag.
    PagePress { page: usize, x: i32, y: i32 },
    Page(PageDrag),
    /// Moving (grip 0, 0) or resizing an overlay of the active tab. `start` is
    /// where the pointer went down, in page points; `orig` the box and font
    /// size then.
    Overlay { index: usize, gx: i8, gy: i8, start: (f32, f32), orig: [f32; 4], size: f32 },
    /// Dragging out a new highlight from `anchor` (page points).
    NewHighlight { index: usize, anchor: (f32, f32) },
    /// Drawing in the signature pad.
    Pad,
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
    bold: HFONT,
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
            bold: gdi::font("Segoe UI", 15, dpi, 600),
        }
    }

    fn delete(&self) {
        for f in [self.ui, self.status, self.badge, self.icon, self.icon_small, self.watermark, self.bold] {
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

/// Deletes page `page` of `tab` with its overlays.
fn take_page(tab: &mut Tab, page: usize) {
    tab.doc.delete_page(page);
    tab.remap_overlays(|p| match p {
        _ if p == page => None,
        _ if p > page => Some(p - 1),
        _ => Some(p),
    });
    tab.pages_changed();
}

/// Minimap geometry for the active tab. `f` maps content px → minimap px.
struct MiniGeo {
    f: f32,
    offset: i32,
    slider: RECT,
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
    tool: Tool,
    sign_menu: bool,
    /// Making a new signature (a modal sheet over everything).
    pad: Option<SignPad>,
    /// Loaded the first time the Sign menu opens.
    library: Option<Library>,
    /// Last pointer position (for the stamp preview).
    mouse: (i32, i32),
    /// First half of a UTF-16 surrogate pair typed into a text box.
    surrogate: Option<u16>,
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
            tool: Tool::Select,
            sign_menu: false,
            pad: None,
            library: None,
            mouse: (-1, -1),
            surrogate: None,
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
        if let Some(tab) = self.active_tab_mut() {
            tab.end_edit();
        }
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
        MiniGeo { f, offset, slider: RECT { left: mm.left, top, right: mm.right, bottom: top + slider_h } }
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
        if self.sign_menu {
            let menu = self.menu_layout();
            if let Some(&(item, _)) = menu.entries.iter().find(|(_, r)| gdi::contains(r, x, y)) {
                return Hit::Menu(item);
            }
            if gdi::contains(&menu.panel, x, y) {
                return Hit::MenuPanel;
            }
        }
        if gdi::contains(&r.file_button, x, y) {
            return Hit::MenuFile;
        }
        if let Some(&(b, _)) = self.tool_rects().iter().find(|(_, r)| gdi::contains(r, x, y)) {
            return Hit::Tool(b);
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
        if let Some(&(b, _)) = self.zoom_rects().iter().find(|(_, r)| gdi::contains(r, x, y)) {
            return Hit::Zoom(b);
        }
        if self.hscroll_thumb().is_some_and(|(track, _, _)| gdi::contains(&track, x, y)) {
            return Hit::HScroll;
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

    /// Tool buttons, right of the File menu.
    fn tool_rects(&self) -> Vec<(ToolButton, RECT)> {
        let fb = self.regions().file_button;
        let mut x = fb.right + self.m.px(14);
        TOOL_BUTTONS
            .iter()
            .map(|&(b, label)| {
                let w = gdi::text_width(self.measure, self.fonts.ui, label) + self.m.px(16);
                let r = RECT { left: x, right: x + w, ..fb };
                x += w + self.m.px(2);
                (b, r)
            })
            .collect()
    }

    fn library(&mut self) -> &mut Library {
        self.library.get_or_insert_with(Library::load)
    }

    /// The Sign menu: signatures, then initials, each a row of tiles ending
    /// in a "new" tile.
    fn menu_layout(&self) -> MenuLayout {
        let m = self.m;
        let (tile_w, tile_h, gap, pad, cols) = (m.px(116), m.px(56), m.px(8), m.px(12), 3);
        let header_h = m.px(28);
        let items = self.library.as_ref().map_or(&[][..], |l| &l.items[..]);
        // As wide as the fullest row (each row ends with a "new" tile).
        let most = [SigKind::Signature, SigKind::Initials].map(|k| items.iter().filter(|s| s.kind == k).count() as i32 + 1);
        let cols = most.into_iter().max().unwrap_or(1).min(cols);
        let w = pad * 2 + cols * tile_w + (cols - 1) * gap;
        let anchor = self.tool_rects().into_iter().find(|(b, _)| *b == ToolButton::Sign).map_or(RECT::default(), |(_, r)| r);
        let left = anchor.left.min(self.size.0 - w - m.px(4)).max(0);
        let mut y = anchor.bottom + m.px(4) + pad / 2;
        let (mut headers, mut entries) = (Vec::new(), Vec::new());
        for (kind, title) in [(SigKind::Signature, "Signatures"), (SigKind::Initials, "Initials")] {
            headers.push((title, rect(left + pad, y, w - 2 * pad, header_h)));
            y += header_h;
            let mut tiles: Vec<MenuItem> =
                items.iter().enumerate().filter(|(_, s)| s.kind == kind).map(|(i, _)| MenuItem::Use(i)).collect();
            tiles.push(MenuItem::New(kind));
            for (n, item) in tiles.into_iter().enumerate() {
                let (col, row) = (n as i32 % cols, n as i32 / cols);
                let r = rect(left + pad + col * (tile_w + gap), y + row * (tile_h + gap), tile_w, tile_h);
                if let MenuItem::Use(i) = item {
                    let x = m.px(18);
                    entries.push((MenuItem::Delete(i), rect(r.right - x - m.px(3), r.top + m.px(3), x, x)));
                }
                entries.push((item, r));
            }
            y = entries.last().map_or(y, |(_, r)| r.bottom) + gap;
        }
        MenuLayout { panel: RECT { left, top: anchor.bottom + m.px(4), right: left + w, bottom: y - gap + pad }, headers, entries }
    }

    // ---- overlays on the page ----------------------------------------------

    /// Where page `i` of `tab` is on screen: top-left corner, and pixels per point.
    fn page_frame(tab: &Tab, editor: &RECT, i: usize) -> (f32, f32, f32) {
        let p = tab.pages()[i];
        let pw = tab.doc.page_sizes.get(i).map_or(1.0, |s| s.0);
        ((editor.left + p.x - tab.scroll_x) as f32, (editor.top + p.y - tab.scroll_y) as f32, p.w as f32 / pw)
    }

    /// The page under an editor point, and that point in page points.
    fn page_point(&self, x: i32, y: i32) -> Option<(usize, f32, f32)> {
        let e = self.regions().editor;
        if !gdi::contains(&e, x, y) {
            return None;
        }
        let tab = self.active_tab()?;
        for i in tab.pages_in(tab.scroll_y, tab.scroll_y + (e.bottom - e.top)) {
            let (ox, oy, s) = Self::page_frame(tab, &e, i);
            let p = tab.pages()[i];
            if gdi::contains(&rect(ox as i32, oy as i32, p.w, p.h), x, y) {
                return Some((i, (x as f32 - ox) / s, (y as f32 - oy) / s));
            }
        }
        None
    }

    /// A client point in page `i`'s points, even off the page (for drags).
    fn to_page_points(&self, i: usize, x: i32, y: i32) -> (f32, f32) {
        let e = self.regions().editor;
        let Some(tab) = self.active_tab().filter(|t| i < t.pages().len()) else { return (0.0, 0.0) };
        let (ox, oy, s) = Self::page_frame(tab, &e, i);
        ((x as f32 - ox) / s, (y as f32 - oy) / s)
    }

    /// Screen rect of overlay `i` of the active tab, if its page is laid out.
    fn overlay_rect(&self, i: usize) -> Option<RECT> {
        let tab = self.active_tab()?;
        let o = tab.overlays.get(i)?;
        if o.page >= tab.pages().len() {
            return None;
        }
        let (ox, oy, s) = Self::page_frame(tab, &self.regions().editor, o.page);
        Some(o.screen_rect(ox, oy, s))
    }

    fn overlay_at(&self, x: i32, y: i32) -> Option<OverlayHit> {
        let tab = self.active_tab()?;
        if let Some(i) = tab.selected.filter(|_| tab.caret.is_none()) {
            let r = self.overlay_rect(i)?;
            let all_sides = !tab.overlays[i].keeps_aspect();
            if let Some(&(gx, gy, _)) = grips(&r, all_sides, self.m).iter().find(|(_, _, g)| gdi::contains(g, x, y)) {
                return Some(OverlayHit::Grip(i, gx, gy));
            }
        }
        let slop = self.m.px(3);
        (0..tab.overlays.len()).rev().find_map(|i| {
            let r = self.overlay_rect(i)?;
            let r = RECT { left: r.left - slop, top: r.top - slop, right: r.right + slop, bottom: r.bottom + slop };
            gdi::contains(&r, x, y).then_some(OverlayHit::Body(i))
        })
    }

    /// Keeps a box on its page (as far as it fits).
    fn clamp_to_page(tab: &Tab, o: &mut Overlay) {
        let (pw, ph) = tab.doc.page_sizes.get(o.page).copied().unwrap_or((612.0, 792.0));
        o.x = o.x.min(pw - o.w).max(0.0_f32.min(pw - o.w));
        o.y = o.y.min(ph - o.h).max(0.0_f32.min(ph - o.h));
    }

    /// The signature preview under the pointer while the stamp tool is armed.
    fn stamp_ghost(&self) -> Option<(Rc<Image>, RECT)> {
        let Tool::Stamp(i) = self.tool else { return None };
        if self.drag != Drag::None || self.sign_menu || self.pad.is_some() {
            return None;
        }
        let (mx, my) = self.mouse;
        if self.overlay_at(mx, my).is_some() {
            return None;
        }
        let (page, _, _) = self.page_point(mx, my)?;
        let sig = self.library.as_ref()?.items.get(i)?;
        let tab = self.active_tab()?;
        let (_, _, s) = Self::page_frame(tab, &self.regions().editor, page);
        let (w, h) = sig.stamp_size();
        let (w, h) = ((w * s) as i32, (h * s) as i32);
        Some((sig.image.clone(), rect(mx - w / 2, my - h / 2, w, h)))
    }

    /// Ends text editing in every tab (before saving or printing).
    pub fn commit_edits(&mut self) {
        for slot in &mut self.tabs {
            if let Slot::Doc(tab) = slot {
                tab.end_edit();
            }
        }
    }

    fn set_tool(&mut self, tool: Tool) {
        if let Some(tab) = self.active_tab_mut() {
            tab.end_edit();
        }
        self.tool = tool;
    }

    // ---- signatures ------------------------------------------------------

    /// A picture chosen for the signature pad.
    pub fn pad_set_image(&mut self, img: Rc<Image>) {
        if let Some(pad) = &mut self.pad {
            pad.set_image(img);
        }
    }

    pub fn delete_signature(&mut self, i: usize) -> Result<(), String> {
        self.library().remove(i)?;
        self.tool = match self.tool {
            Tool::Stamp(j) if j == i => Tool::Select,
            Tool::Stamp(j) if j > i => Tool::Stamp(j - 1),
            t => t,
        };
        Ok(())
    }

    fn pad_event(&mut self, event: PadEvent) -> Action {
        match event {
            PadEvent::None => Action::None,
            PadEvent::Redraw => Action::Redraw,
            PadEvent::Capture => {
                self.drag = Drag::Pad;
                Action::Capture
            }
            PadEvent::Release => {
                self.drag = Drag::None;
                Action::Release(None)
            }
            PadEvent::Import => Action::ImportSignature,
            PadEvent::Cancel => {
                self.pad = None;
                Action::Redraw
            }
            PadEvent::Save(img) => {
                let Some(kind) = self.pad.as_ref().map(|p| p.kind) else { return Action::None };
                match self.library().add(kind, img) {
                    Ok(i) => {
                        self.pad = None;
                        self.set_tool(Tool::Stamp(i));
                        Action::Redraw
                    }
                    Err(e) => Action::Error(e),
                }
            }
        }
    }

    fn menu_click(&mut self, item: MenuItem) -> Action {
        self.sign_menu = false;
        match item {
            MenuItem::Use(i) => {
                self.set_tool(Tool::Stamp(i));
                Action::Redraw
            }
            MenuItem::New(kind) => {
                self.pad = Some(SignPad::new(kind));
                Action::Redraw
            }
            MenuItem::Delete(i) => Action::DeleteSignature(i),
        }
    }

    fn tool_click(&mut self, b: ToolButton) -> Action {
        if self.active_tab().is_none() {
            return Action::None;
        }
        match b {
            ToolButton::Select => self.set_tool(Tool::Select),
            ToolButton::Text => self.set_tool(Tool::Text),
            ToolButton::Highlight => self.set_tool(Tool::Highlight),
            ToolButton::Sign => {
                self.library();
                self.sign_menu = !self.sign_menu;
            }
        }
        Action::Redraw
    }

    // ---- zoom --------------------------------------------------------------

    /// Sets the active tab's zoom, keeping the document point under client
    /// point `anchor` (default: the middle of the page view) where it is.
    pub fn set_zoom(&mut self, zoom: f32, anchor: Option<(i32, i32)>) -> Action {
        self.sync_layout();
        let e = self.regions().editor;
        let (ax, ay) = anchor.unwrap_or(((e.left + e.right) / 2, (e.top + e.bottom) / 2));
        let (vx, vy) = (ax - e.left, ay - e.top);
        let m = self.m;
        let Some(tab) = self.active_tab_mut() else { return Action::None };
        let zoom = zoom.clamp(ZOOM_MIN, ZOOM_MAX);
        if zoom == tab.zoom || tab.pages().is_empty() {
            return Action::None;
        }
        // Remember the anchor as a fraction of the page under (or nearest) it.
        let (cx, cy) = (tab.scroll_x + vx, tab.scroll_y + vy);
        let i = tab.pages().iter().position(|p| p.y + p.h >= cy).unwrap_or(tab.pages().len() - 1);
        let p = tab.pages()[i];
        let (fx, fy) = ((cx - p.x) as f32 / p.w as f32, (cy - p.y) as f32 / p.h as f32);
        tab.zoom = zoom;
        let view = (e.right - e.left, e.bottom - e.top);
        tab.ensure_layout(view.0, m);
        let p = tab.pages()[i];
        tab.scroll_x_to(p.x + (fx * p.w as f32).round() as i32 - vx, view.0);
        tab.scroll_to(p.y + (fy * p.h as f32).round() as i32 - vy, view.1);
        Action::Redraw
    }

    /// One zoom step in (`dir` > 0) or out.
    pub fn zoom_step(&mut self, dir: i32) -> Action {
        let Some(z) = self.active_tab().map(|t| t.zoom) else { return Action::None };
        let next = if dir > 0 {
            ZOOM_STEPS.iter().copied().find(|&s| s > z + 0.001).unwrap_or(ZOOM_MAX)
        } else {
            ZOOM_STEPS.iter().rev().copied().find(|&s| s < z - 0.001).unwrap_or(ZOOM_MIN)
        };
        self.set_zoom(next, None)
    }

    pub fn zoom_reset(&mut self) -> Action {
        self.set_zoom(ZOOM_DEFAULT, None)
    }

    /// Zoom controls in the status bar: − 75% +, left of the page size.
    fn zoom_rects(&self) -> Vec<(ZoomButton, RECT)> {
        let Some(tab) = self.active_tab() else { return Vec::new() };
        let r = self.regions();
        let s = r.status;
        let m = self.m;
        let size_w = gdi::text_width(self.measure, self.fonts.status, &self.size_label(tab, &r));
        let right = s.right - m.px(12) - size_w - m.px(20);
        let (bw, pw) = (m.px(22), m.px(48));
        let (top, h) = (s.top + 1, s.bottom - s.top - 1);
        vec![
            (ZoomButton::Out, rect(right - 2 * bw - pw, top, bw, h)),
            (ZoomButton::Reset, rect(right - bw - pw, top, pw, h)),
            (ZoomButton::In, rect(right - bw, top, bw, h)),
        ]
    }

    /// "8.50 × 11.00 in      PDF" for the current page.
    fn size_label(&self, tab: &Tab, r: &Regions) -> String {
        let current = tab.current_page(r.editor.bottom - r.editor.top);
        let (pw, ph) = tab.doc.page_sizes.get(current).copied().unwrap_or_default();
        format!("{:.2} × {:.2} in      PDF", pw / 72.0, ph / 72.0)
    }

    /// Horizontal scrollbar when pages are wider than the view:
    /// (track, thumb, travel in px).
    fn hscroll_thumb(&self) -> Option<(RECT, RECT, i32)> {
        let tab = self.active_tab()?;
        let e = self.regions().editor;
        let view_w = e.right - e.left;
        let max = tab.max_scroll_x(view_w);
        if max == 0 {
            return None;
        }
        let h = self.m.px(t::SCROLLBAR_W);
        let track = rect(e.left, e.bottom - h, view_w, h);
        let tw = ((view_w as i64 * view_w as i64 / tab.content_w().max(1) as i64) as i32).clamp(self.m.px(20).min(view_w), view_w);
        let travel = view_w - tw;
        let left = e.left + (tab.scroll_x as i64 * travel as i64 / max as i64) as i32;
        let pad = self.m.px(3);
        Some((track, RECT { left, top: track.top + pad, right: left + tw, bottom: track.bottom - pad }, travel))
    }

    fn scroll_x_by(&mut self, dx: i32) -> Action {
        let e = self.regions().editor;
        let Some(tab) = self.active_tab_mut() else { return Action::None };
        if tab.scroll_x_to(tab.scroll_x.saturating_add(dx), e.right - e.left) {
            Action::Redraw
        } else {
            Action::None
        }
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
                let copies = tab.overlays_on(d.page);
                tab.remap_overlays(|p| Some(if p >= at { p + 1 } else { p }));
                tab.add_overlays(copies, at);
            } else {
                // `at` counts the page itself, so past it the final index is one less.
                let (from, dest) = (d.page, if at > d.page { at - 1 } else { at });
                if dest == from {
                    return Ok(());
                }
                tab.doc.move_page(from, dest)?;
                tab.remap_overlays(|p| {
                    Some(match p {
                        _ if p == from => dest,
                        _ if from < dest && p > from && p <= dest => p - 1,
                        _ if dest < from && p >= dest && p < from => p + 1,
                        _ => p,
                    })
                });
            }
            tab.pages_changed();
            return Ok(());
        }
        let Some((src, dst)) = self.two_doc_tabs(d.from, to) else { return Ok(()) };
        check_can_take(src, d)?;
        dst.doc.import_page(&src.doc, d.page, at)?;
        dst.remap_overlays(|p| Some(if p >= at { p + 1 } else { p }));
        dst.add_overlays(src.overlays_on(d.page), at);
        dst.pages_changed();
        if !d.copy {
            take_page(src, d.page);
        }
        Ok(())
    }

    /// Puts the dragged page in a new untitled document: into welcome tab
    /// `slot`, or a new tab at the end.
    fn extract(&mut self, d: PageDrag, slot: Option<usize>) -> Result<(), String> {
        let Some(src) = self.doc_tab_mut(d.from) else { return Ok(()) };
        check_can_take(src, d)?;
        let doc = Document::from_page(&src.doc, d.page)?;
        let copies = src.overlays_on(d.page);
        if !d.copy {
            take_page(src, d.page);
        }
        let mut tab = self.make_tab(doc);
        tab.add_overlays(copies, 0);
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
        if self.pad.is_some() {
            if key == VK_ESCAPE && self.drag == Drag::None {
                self.pad = None;
                return Action::Redraw;
            }
            return Action::None;
        }
        if key == VK_ESCAPE && self.sign_menu {
            self.sign_menu = false;
            return Action::Redraw;
        }
        if key == VK_ESCAPE && matches!(self.drag, Drag::Page(_) | Drag::PagePress { .. }) {
            self.drag = Drag::None;
            return Action::Release(None);
        }
        if self.active_tab().is_some_and(|t| t.caret.is_some()) {
            return self.edit_key(key);
        }
        if self.drag != Drag::None {
            return Action::None;
        }
        let shift = unsafe { GetKeyState(VK_SHIFT.0 as i32) < 0 };
        if let Some(tab) = self.active_tab_mut() {
            if let Some(i) = tab.selected {
                let step = if shift { 10.0 } else { 1.0 };
                let (dx, dy) = match key {
                    VK_LEFT => (-step, 0.0),
                    VK_RIGHT => (step, 0.0),
                    VK_UP => (0.0, -step),
                    VK_DOWN => (0.0, step),
                    _ => (0.0, 0.0),
                };
                if (dx, dy) != (0.0, 0.0) {
                    let mut o = tab.overlays[i].clone();
                    (o.x, o.y) = (o.x + dx, o.y + dy);
                    Self::clamp_to_page(tab, &mut o);
                    tab.overlays[i] = o;
                    tab.dirty = true;
                    return Action::Redraw;
                }
                if key == VK_DELETE || key == VK_BACK {
                    tab.remove_overlay(i);
                    return Action::Redraw;
                }
                if key == VK_ESCAPE {
                    tab.selected = None;
                    return Action::Redraw;
                }
            }
        }
        if key == VK_ESCAPE && self.tool != Tool::Select {
            self.set_tool(Tool::Select);
            return Action::Redraw;
        }
        let line = self.m.px(t::LINE_STEP);
        let page = self.view_h() - line;
        match key {
            VK_UP => self.scroll_by(-line),
            VK_DOWN => self.scroll_by(line),
            VK_LEFT => self.scroll_x_by(-line),
            VK_RIGHT => self.scroll_x_by(line),
            VK_PRIOR => self.scroll_by(-page),
            VK_NEXT | VK_SPACE => self.scroll_by(page),
            VK_HOME => self.scroll_to(0),
            VK_END => self.scroll_to(i32::MAX),
            _ => Action::None,
        }
    }

    /// Caret movement and editing keys while typing in a text box. Other keys
    /// are swallowed so they don't scroll; characters arrive via `char_input`.
    fn edit_key(&mut self, key: VIRTUAL_KEY) -> Action {
        let Some(tab) = self.active_tab_mut() else { return Action::None };
        if key == VK_ESCAPE {
            tab.end_edit();
            return Action::Redraw;
        }
        let handled = tab.edit_text(|text, caret| {
            let line_start = text[..*caret].rfind('\n').map_or(0, |i| i + 1);
            let line_end = text[*caret..].find('\n').map_or(text.len(), |i| *caret + i);
            match key {
                VK_LEFT => *caret -= text[..*caret].chars().next_back().map_or(0, char::len_utf8),
                VK_RIGHT => *caret += text[*caret..].chars().next().map_or(0, char::len_utf8),
                VK_HOME => *caret = line_start,
                VK_END => *caret = line_end,
                VK_DELETE => {
                    if let Some(c) = text[*caret..].chars().next() {
                        text.replace_range(*caret..*caret + c.len_utf8(), "");
                        return true;
                    }
                }
                _ => {}
            }
            false
        });
        if handled && matches!(key, VK_LEFT | VK_RIGHT | VK_HOME | VK_END | VK_DELETE) {
            Action::Redraw
        } else {
            Action::None
        }
    }

    /// A typed character (WM_CHAR, UTF-16 unit) for the text box being edited.
    pub fn char_input(&mut self, unit: u16) -> Action {
        if self.pad.is_some() {
            return Action::None;
        }
        let c = match unit {
            0xD800..=0xDBFF => {
                self.surrogate = Some(unit);
                return Action::None;
            }
            0xDC00..=0xDFFF => match self.surrogate.take() {
                Some(high) => char::decode_utf16([high, unit]).next().and_then(Result::ok),
                None => None,
            },
            _ => char::from_u32(unit as u32),
        };
        let Some(c) = c else { return Action::None };
        let Some(tab) = self.active_tab_mut() else { return Action::None };
        let changed = tab.edit_text(|text, caret| match c {
            '\u{8}' => match text[..*caret].chars().next_back() {
                Some(prev) => {
                    *caret -= prev.len_utf8();
                    text.replace_range(*caret..*caret + prev.len_utf8(), "");
                    true
                }
                None => false,
            },
            '\r' => {
                text.insert(*caret, '\n');
                *caret += 1;
                true
            }
            c if c.is_control() => false,
            c => {
                text.insert(*caret, c);
                *caret += c.len_utf8();
                true
            }
        });
        if changed {
            Action::Redraw
        } else {
            Action::None
        }
    }

    pub fn wheel(&mut self, x: i32, y: i32, delta: i32) -> Action {
        if matches!(self.hit_test(x, y), Hit::TabStrip | Hit::Tab(_) | Hit::TabClose(_)) {
            self.tab_scroll -= delta * self.m.px(40) / 120;
            self.clamp_tab_scroll();
            return Action::Redraw;
        }
        if ctrl_down() {
            let z = self.active_tab().map_or(ZOOM_DEFAULT, |t| t.zoom);
            return self.set_zoom(z * 1.1f32.powf(delta as f32 / 120.0), Some((x, y)));
        }
        let step = -delta * self.m.px(t::LINE_STEP) * 3 / 120;
        if unsafe { GetKeyState(VK_SHIFT.0 as i32) < 0 } {
            return self.scroll_x_by(step);
        }
        self.scroll_by(step)
    }

    /// Tilt wheel / touchpad sideways scrolling (WM_MOUSEHWHEEL).
    pub fn hwheel(&mut self, delta: i32) -> Action {
        self.scroll_x_by(delta * self.m.px(t::LINE_STEP) * 3 / 120)
    }

    pub fn mouse_move(&mut self, x: i32, y: i32) -> Action {
        self.sync_layout();
        let ghost_before = self.stamp_ghost().map(|g| g.1);
        self.mouse = (x, y);
        if let Some(pad) = &mut self.pad {
            return if pad.mouse_move(self.m, self.size, x, y) { Action::Redraw } else { Action::None };
        }
        match self.drag {
            Drag::Overlay { index, gx, gy, start, orig, size } => {
                let Some(page) = self.active_tab().and_then(|t| t.overlays.get(index)).map(|o| o.page) else { return Action::None };
                let (px, py) = self.to_page_points(page, x, y);
                let (dx, dy) = (px - start.0, py - start.1);
                let Some(tab) = self.active_tab_mut() else { return Action::None };
                let mut o = tab.overlays[index].clone();
                if (gx, gy) == (0, 0) {
                    (o.x, o.y) = (orig[0] + dx, orig[1] + dy);
                } else {
                    let [nx, ny, nw, nh] = resize_box(orig, gx, gy, dx, dy, o.keeps_aspect());
                    (o.x, o.y, o.w, o.h) = (nx, ny, nw, nh);
                    if let Kind::Text { size: s, .. } = &mut o.kind {
                        // Text scales its font; the box follows from that.
                        *s = (size * nh / orig[3]).max(2.0);
                        o.fit_text();
                        if gx < 0 {
                            o.x = orig[0] + orig[2] - o.w;
                        }
                        if gy < 0 {
                            o.y = orig[1] + orig[3] - o.h;
                        }
                    }
                }
                Self::clamp_to_page(tab, &mut o);
                let old = &tab.overlays[index];
                if (o.x, o.y, o.w, o.h) == (old.x, old.y, old.w, old.h) {
                    return Action::None;
                }
                tab.overlays[index] = o;
                tab.dirty = true;
                return Action::Redraw;
            }
            Drag::NewHighlight { index, anchor } => {
                let Some(page) = self.active_tab().and_then(|t| t.overlays.get(index)).map(|o| o.page) else { return Action::None };
                let (px, py) = self.to_page_points(page, x, y);
                let Some(tab) = self.active_tab_mut() else { return Action::None };
                let (pw, ph) = tab.doc.page_sizes.get(page).copied().unwrap_or((612.0, 792.0));
                let (px, py) = (px.clamp(0.0, pw), py.clamp(0.0, ph));
                let o = &mut tab.overlays[index];
                (o.x, o.y, o.w, o.h) = (anchor.0.min(px), anchor.1.min(py), (px - anchor.0).abs(), (py - anchor.1).abs());
                tab.dirty = true;
                return Action::Redraw;
            }
            Drag::Pad => return Action::None,
            Drag::HScroll { grab } => {
                let Some((track, _, travel)) = self.hscroll_thumb() else { return Action::None };
                let view_w = track.right - track.left;
                let Some(max) = self.active_tab().map(|t| t.max_scroll_x(view_w)) else { return Action::None };
                let pos = (x - grab - track.left) as i64 * max as i64 / travel.max(1) as i64;
                let Some(tab) = self.active_tab_mut() else { return Action::None };
                return if tab.scroll_x_to(pos as i32, view_w) { Action::Redraw } else { Action::None };
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
        let ghost_moved = self.stamp_ghost().map(|g| g.1) != ghost_before;
        if hit != self.hover || ghost_moved {
            self.hover = hit;
            return Action::Redraw;
        }
        Action::None
    }

    /// Pointer shape for a client point.
    pub fn cursor(&self, x: i32, y: i32) -> Cursor {
        if let Some(pad) = &self.pad {
            return match pad.hit(self.m, self.size, x, y) {
                crate::signpad::PadHit::Canvas => Cursor::Cross,
                crate::signpad::PadHit::None => Cursor::Arrow,
                _ => Cursor::Hand,
            };
        }
        match self.hit_test(x, y) {
            Hit::Menu(_) => return Cursor::Hand,
            Hit::Editor => {}
            _ => return Cursor::Arrow,
        }
        let editing = self.active_tab().and_then(|t| t.caret.and(t.selected));
        match self.overlay_at(x, y) {
            Some(OverlayHit::Grip(_, gx, gy)) => match (gx, gy) {
                (0, _) => Cursor::SizeNS,
                (_, 0) => Cursor::SizeWE,
                _ if gx == gy => Cursor::SizeNWSE,
                _ => Cursor::SizeNESW,
            },
            Some(OverlayHit::Body(i)) if editing == Some(i) => Cursor::IBeam,
            Some(OverlayHit::Body(_)) => Cursor::Move,
            None if self.page_point(x, y).is_none() => Cursor::Arrow,
            None => match self.tool {
                Tool::Select => Cursor::Arrow,
                Tool::Text => Cursor::IBeam,
                Tool::Highlight | Tool::Stamp(_) => Cursor::Cross,
            },
        }
    }

    pub fn mouse_leave(&mut self) -> Action {
        if self.drag != Drag::None {
            return Action::None;
        }
        let had_ghost = self.stamp_ghost().is_some();
        self.mouse = (-1, -1);
        if self.hover != Hit::None || had_ghost {
            self.hover = Hit::None;
            return Action::Redraw;
        }
        Action::None
    }

    pub fn left_down(&mut self, x: i32, y: i32) -> Action {
        self.sync_layout();
        self.mouse = (x, y);
        if let Some(pad) = &mut self.pad {
            let event = pad.left_down(self.m, self.size, x, y);
            return self.pad_event(event);
        }
        let r = self.regions();
        let hit = self.hit_test(x, y);
        if self.sign_menu && !matches!(hit, Hit::Menu(_) | Hit::MenuPanel | Hit::Tool(ToolButton::Sign)) {
            self.sign_menu = false; // a click elsewhere just closes the menu
            return Action::Redraw;
        }
        match hit {
            Hit::Menu(item) => self.menu_click(item),
            Hit::MenuPanel => Action::None,
            Hit::Tool(b) => self.tool_click(b),
            Hit::Editor => self.editor_down(x, y),
            Hit::MenuFile => Action::FileMenu(r.file_button.left, r.file_button.bottom),
            Hit::TabClose(i) => Action::CloseTab(i),
            Hit::Tab(i) => {
                self.activate(i);
                Action::Redraw
            }
            // A page: click to jump, drag to rearrange. The slider only marks
            // what's on screen; scrolling is the scrollbar's job.
            Hit::Minimap => match self.page_at(x, y) {
                Some(page) => {
                    self.drag = Drag::PagePress { page, x, y };
                    Action::Capture
                }
                None => Action::None,
            },
            Hit::Zoom(ZoomButton::Out) => self.zoom_step(-1),
            Hit::Zoom(ZoomButton::In) => self.zoom_step(1),
            Hit::Zoom(ZoomButton::Reset) => self.zoom_reset(),
            Hit::HScroll => {
                let Some((track, thumb, _)) = self.hscroll_thumb() else { return Action::None };
                let page = track.right - track.left - self.m.px(t::LINE_STEP);
                if x < thumb.left {
                    self.scroll_x_by(-page)
                } else if x >= thumb.right {
                    self.scroll_x_by(page)
                } else {
                    self.drag = Drag::HScroll { grab: x - thumb.left };
                    Action::Capture
                }
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

    /// A press in the page view: grab an overlay, or use the current tool.
    fn editor_down(&mut self, x: i32, y: i32) -> Action {
        // Clicking inside the text box being typed in keeps typing.
        if let (Some(OverlayHit::Body(i)), Some(tab)) = (self.overlay_at(x, y), self.active_tab()) {
            if tab.caret.is_some() && tab.selected == Some(i) {
                return Action::None;
            }
        }
        let Some(tab) = self.active_tab_mut() else { return Action::None };
        tab.end_edit(); // may remove an empty text box, so hit-test after
        let grab = |app: &mut Self, index: usize, gx: i8, gy: i8| {
            let Some(page) = app.active_tab().map(|t| t.overlays[index].page) else { return Action::None };
            let start = app.to_page_points(page, x, y);
            let Some(tab) = app.active_tab_mut() else { return Action::None };
            let o = &tab.overlays[index];
            let size = match o.kind {
                Kind::Text { size, .. } => size,
                _ => 0.0,
            };
            let orig = [o.x, o.y, o.w, o.h];
            tab.selected = Some(index);
            app.drag = Drag::Overlay { index, gx, gy, start, orig, size };
            Action::Capture
        };
        match self.overlay_at(x, y) {
            Some(OverlayHit::Grip(i, gx, gy)) => return grab(self, i, gx, gy),
            Some(OverlayHit::Body(i)) => return grab(self, i, 0, 0),
            None => {}
        }
        let Some((page, px, py)) = self.page_point(x, y) else {
            if let Some(tab) = self.active_tab_mut() {
                tab.selected = None;
            }
            return Action::Redraw;
        };
        let stamp = match self.tool {
            Tool::Stamp(i) => self.library.as_ref().and_then(|l| l.items.get(i)).map(|s| (s.image.clone(), s.stamp_size())),
            _ => None,
        };
        let tool = self.tool;
        let Some(tab) = self.active_tab_mut() else { return Action::None };
        tab.selected = None;
        match tool {
            Tool::Select => Action::Redraw,
            Tool::Text => {
                // The click lands in the middle of the first line.
                let mut o = Overlay::text(page, px, py - TEXT_SIZE * overlay::LINE / 2.0, String::new(), TEXT_SIZE);
                Self::clamp_to_page(tab, &mut o);
                tab.overlays.push(o);
                tab.selected = Some(tab.overlays.len() - 1);
                tab.caret = Some(0);
                Action::Redraw
            }
            Tool::Highlight => {
                tab.overlays.push(Overlay { page, x: px, y: py, w: 0.0, h: 0.0, kind: Kind::Highlight });
                let index = tab.overlays.len() - 1;
                tab.selected = Some(index);
                self.drag = Drag::NewHighlight { index, anchor: (px, py) };
                Action::Capture
            }
            Tool::Stamp(_) => {
                let Some((img, (w, h))) = stamp else { return Action::None };
                let mut o = Overlay { page, x: px - w / 2.0, y: py - h / 2.0, w, h, kind: Kind::Image(img) };
                Self::clamp_to_page(tab, &mut o);
                tab.overlays.push(o);
                tab.selected = Some(tab.overlays.len() - 1);
                tab.dirty = true;
                Action::Redraw
            }
        }
    }

    /// Double-clicking text edits it; anywhere else it's a second click.
    pub fn double_click(&mut self, x: i32, y: i32) -> Action {
        self.sync_layout();
        if self.pad.is_none() && !self.sign_menu && self.hit_test(x, y) == Hit::Editor {
            if let Some(OverlayHit::Body(i)) = self.overlay_at(x, y) {
                if let Some(tab) = self.active_tab_mut() {
                    if let Kind::Text { text, .. } = &tab.overlays[i].kind {
                        tab.caret = Some(text.len());
                        tab.selected = Some(i);
                        return Action::Redraw;
                    }
                }
            }
        }
        self.left_down(x, y)
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
        if let Some(pad) = &mut self.pad {
            let event = pad.left_up();
            return self.pad_event(event);
        }
        match std::mem::replace(&mut self.drag, Drag::None) {
            Drag::None | Drag::Pad => Action::None,
            Drag::Overlay { .. } => Action::Release(None),
            Drag::NewHighlight { index, .. } => {
                // A click without a drag doesn't leave a speck behind.
                if let Some(tab) = self.active_tab_mut() {
                    if tab.overlays.get(index).is_some_and(|o| o.w < 3.0 || o.h < 3.0) {
                        tab.overlays.remove(index);
                        tab.selected = None;
                    }
                }
                Action::Release(None)
            }
            Drag::PagePress { page, .. } => {
                // A click on a page: bring it to the top of the view.
                let margin = self.m.px(t::PAGE_MARGIN);
                if let Some(y) = self.active_tab().and_then(|tab| tab.pages().get(page)).map(|p| p.y) {
                    self.scroll_to(y - margin);
                }
                Action::Release(None)
            }
            Drag::Page(d) => Action::Release(self.drop_page(d).err()),
            Drag::Scrollbar { .. } | Drag::HScroll { .. } => Action::Release(None),
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
        if let Some(pad) = &mut self.pad {
            pad.capture_lost();
        }
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
            if let Some((img, g)) = self.stamp_ghost() {
                img.draw(dc, &g, 0x99);
            }
            if let Some((_, thumb, _)) = self.hscroll_thumb() {
                let alpha = match (self.drag, self.hover) {
                    (Drag::HScroll { .. }, _) => t::SLIDER_ALPHA_ACTIVE,
                    (_, Hit::HScroll) => t::SLIDER_ALPHA_HOVER,
                    _ => t::SLIDER_ALPHA_HOVER - 0x10,
                };
                self.tint.fill(dc, &thumb, t::SLIDER, alpha);
            }
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
        if self.sign_menu {
            self.paint_sign_menu(dc);
        }
        self.paint_drag_ghost(dc);
        if let Some(pad) = &self.pad {
            pad.paint(dc, self.m, self.size, &self.tint, self.fonts.ui, self.fonts.bold);
        }
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
        let e = r.editor;
        self.with_view(|tab, view_h| {
            // Re-clamp after resizes and zooms.
            tab.scroll_to(tab.scroll_y, view_h);
            tab.scroll_x_to(tab.scroll_x, e.right - e.left);
            let top = tab.scroll_y;
            let visible = tab.pages_in(top, top + view_h);
            for i in visible.clone() {
                let p = tab.pages()[i];
                let (x, y) = (e.left + p.x - tab.scroll_x, e.top + p.y - top);
                gdi::fill(dc, &rect(x + shadow, y + shadow, p.w, p.h), t::PAGE_SHADOW);
                gdi::fill(dc, &rect(x, y, p.w, p.h), t::WHITE);
                // The part of the page on screen, in the page's own pixels.
                let shown = (e.left.max(x) - x, e.top.max(y) - y, e.right.min(x + p.w) - x, e.bottom.min(y + p.h) - y);
                if let Some(bmp) = tab.page(i, p.w, shown, render) {
                    // 1:1 normally; stretched while a window-edge drag skips rendering.
                    let k = p.w as f32 / bmp.page_w as f32;
                    let at = |v: i32| (v as f32 * k).round() as i32;
                    gdi::blit(dc, bmp, x + at(bmp.x0), y + at(bmp.y0), at(bmp.width), at(bmp.height));
                }
            }
            // Overlays after all pages: rendering a page is what reads its saved ones.
            for o in tab.overlays.iter().filter(|o| visible.contains(&o.page)) {
                let (ox, oy, s) = Self::page_frame(tab, &r.editor, o.page);
                o.paint(dc, ox, oy, s, 255);
            }
            let Some(o) = tab.selected.and_then(|i| tab.overlays.get(i)).filter(|o| visible.contains(&o.page)) else { return };
            let (ox, oy, s) = Self::page_frame(tab, &r.editor, o.page);
            let b = o.screen_rect(ox, oy, s);
            let frame = RECT { left: b.left - 1, top: b.top - 1, right: b.right + 1, bottom: b.bottom + 1 };
            for edge in [
                rect(frame.left, frame.top, frame.right - frame.left, 1),
                rect(frame.left, frame.bottom - 1, frame.right - frame.left, 1),
                rect(frame.left, frame.top, 1, frame.bottom - frame.top),
                rect(frame.right - 1, frame.top, 1, frame.bottom - frame.top),
            ] {
                gdi::fill(dc, &edge, t::ACCENT);
            }
            match (tab.caret, &o.kind) {
                (Some(c), Kind::Text { text, size }) => {
                    let line = text[..c].matches('\n').count();
                    let start = text[..c].rfind('\n').map_or(0, |i| i + 1);
                    let x = b.left + (overlay::text_width(&text[start..c], *size) * s).round() as i32;
                    let y = b.top + (line as f32 * overlay::LINE * size * s).round() as i32;
                    let h = (size * overlay::LINE * s).round().max(2.0) as i32;
                    gdi::fill(dc, &rect(x, y, (m.px(1)).max(1), h), t::rgb(0x000000));
                }
                _ => {
                    for (_, _, g) in grips(&b, !o.keeps_aspect(), m) {
                        gdi::fill(dc, &g, t::ACCENT);
                        gdi::fill(dc, &RECT { left: g.left + 1, top: g.top + 1, right: g.right - 1, bottom: g.bottom - 1 }, t::WHITE);
                    }
                }
            }
        });
    }

    fn paint_minimap(&mut self, dc: HDC, r: &Regions) {
        let mm = r.minimap;
        let view_h = r.editor.bottom - r.editor.top;
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
        for o in &tab.overlays {
            if let Some(k) = visible.clone().position(|i| i == o.page) {
                let r = rects[k];
                let pw = tab.doc.page_sizes.get(o.page).map_or(1.0, |s| s.0);
                o.paint(dc, r.left as f32, r.top as f32, (r.right - r.left) as f32 / pw, 255);
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
            self.tint.fill(dc, &geo.slider, t::SLIDER, t::SLIDER_ALPHA);
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

        let enabled = self.active_tab().is_some();
        let rects = self.tool_rects();
        if let Some((_, first)) = rects.first() {
            let sep = rect(first.left - self.m.px(8), first.top + self.m.px(4), 1, first.bottom - first.top - self.m.px(8));
            gdi::fill(dc, &sep, t::BORDER);
        }
        for (b, br) in rects {
            let active = enabled
                && match b {
                    ToolButton::Select => self.tool == Tool::Select,
                    ToolButton::Text => self.tool == Tool::Text,
                    ToolButton::Highlight => self.tool == Tool::Highlight,
                    ToolButton::Sign => matches!(self.tool, Tool::Stamp(_)) || self.sign_menu,
                };
            if active || (enabled && self.hover == Hit::Tool(b)) {
                gdi::round_fill(dc, &br, self.m.px(5), t::HOVER_BG);
            }
            if active {
                let bar = rect(br.left + self.m.px(6), br.bottom - self.m.px(2), br.right - br.left - self.m.px(12), self.m.px(2));
                gdi::fill(dc, &bar, t::ACCENT);
            }
            let label = TOOL_BUTTONS.iter().find(|(k, _)| *k == b).map_or("", |(_, l)| l);
            let color = if !enabled { t::TEXT_FAINT } else if active { t::TEXT_BRIGHT } else { t::TEXT };
            gdi::text(dc, self.fonts.ui, color, label, br, DT_CENTER | DT_VCENTER);
        }
    }

    fn paint_sign_menu(&self, dc: HDC) {
        let m = self.m;
        let menu = self.menu_layout();
        gdi::fill(dc, &menu.panel, t::BORDER);
        let inner = RECT { left: menu.panel.left + 1, top: menu.panel.top + 1, right: menu.panel.right - 1, bottom: menu.panel.bottom - 1 };
        gdi::fill(dc, &inner, t::rgb(0x202020));
        for (title, r) in &menu.headers {
            gdi::text(dc, self.fonts.status, t::TEXT_DIM, title, *r, DT_LEFT | DT_VCENTER);
        }
        let items = self.library.as_ref().map_or(&[][..], |l| &l.items[..]);
        let hovered = |i: usize| matches!(self.hover, Hit::Menu(MenuItem::Use(h) | MenuItem::Delete(h)) if h == i);
        for &(item, r) in &menu.entries {
            match item {
                MenuItem::Use(i) => {
                    let selected = self.tool == Tool::Stamp(i);
                    if selected || hovered(i) {
                        let ring = RECT { left: r.left - 2, top: r.top - 2, right: r.right + 2, bottom: r.bottom + 2 };
                        gdi::round_fill(dc, &ring, m.px(6), t::ACCENT);
                    }
                    gdi::round_fill(dc, &r, m.px(4), t::WHITE);
                    if let Some(sig) = items.get(i) {
                        let pad = m.px(6);
                        let (aw, ah) = ((r.right - r.left - 2 * pad) as f32, (r.bottom - r.top - 2 * pad) as f32);
                        let k = (aw / sig.image.w as f32).min(ah / sig.image.h as f32);
                        let (w, h) = ((sig.image.w as f32 * k) as i32, (sig.image.h as f32 * k) as i32);
                        let x = r.left + (r.right - r.left - w) / 2;
                        let y = r.top + (r.bottom - r.top - h) / 2;
                        sig.image.draw(dc, &rect(x, y, w, h), 255);
                    }
                }
                MenuItem::New(kind) => {
                    let on = self.hover == Hit::Menu(item);
                    gdi::round_fill(dc, &r, m.px(4), if on { t::rgb(0x3C3C3C) } else { t::KEY_BG });
                    let label = match kind {
                        SigKind::Signature => "+ New signature",
                        SigKind::Initials => "+ New initials",
                    };
                    gdi::text(dc, self.fonts.status, if on { t::TEXT_BRIGHT } else { t::TEXT }, label, r, DT_CENTER | DT_VCENTER);
                }
                MenuItem::Delete(_) => {}
            }
        }
        // Delete buttons on top of their tiles, shown while hovering the tile.
        for &(item, r) in &menu.entries {
            if let MenuItem::Delete(i) = item {
                if hovered(i) {
                    let on = self.hover == Hit::Menu(item);
                    gdi::round_fill(dc, &r, r.right - r.left, if on { t::PDF_ICON } else { t::rgb(0x505050) });
                    gdi::text(dc, self.fonts.icon_small, t::TEXT_BRIGHT, t::ICON_CLOSE, r, DT_CENTER | DT_VCENTER);
                }
            }
        }
    }

    /// One line on what the current tool or selection does.
    fn hint(&self) -> Option<&'static str> {
        let tab = self.active_tab()?;
        if tab.caret.is_some() {
            return Some("Typing  ·  Enter: new line  ·  Esc or click outside: done");
        }
        if self.pad.is_some() {
            return None;
        }
        Some(match self.tool {
            Tool::Text => "Click on a page to add text  ·  Double-click text to edit it  ·  Esc: done",
            Tool::Highlight => "Drag across a page to highlight  ·  Esc: done",
            Tool::Stamp(_) => "Click to place  ·  Drag to move, corners to resize  ·  Esc: done",
            Tool::Select if tab.selected.is_some() => "Drag to move  ·  Drag handles to resize  ·  Arrows: nudge  ·  Delete: remove",
            Tool::Select => return None,
        })
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
        let mut left = format!("Page {} of {}", current + 1, tab.doc.page_count());
        if let Some(hint) = self.hint() {
            left = format!("{left}      {hint}");
        }
        let left_rect = RECT { left: pad, right: self.zoom_rects().first().map_or(s.right, |(_, z)| z.left) - pad, ..s };
        gdi::text(dc, self.fonts.status, t::TEXT, &left, left_rect, DT_LEFT | DT_VCENTER | DT_END_ELLIPSIS);
        let right = self.size_label(tab, r);
        gdi::text(dc, self.fonts.status, t::TEXT, &right, RECT { right: s.right - pad, ..s }, DT_RIGHT | DT_VCENTER);
        for (b, br) in self.zoom_rects() {
            if self.hover == Hit::Zoom(b) {
                gdi::fill(dc, &br, t::HOVER_BG);
            }
            let label = match b {
                ZoomButton::Out => "\u{2212}".to_string(),
                ZoomButton::In => "+".to_string(),
                ZoomButton::Reset => format!("{:.0}%", tab.zoom * 100.0),
            };
            gdi::text(dc, self.fonts.status, t::TEXT, &label, br, DT_CENTER | DT_VCENTER);
        }
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

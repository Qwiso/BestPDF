//! One open document: its scroll position, page layout and rendered bitmaps.
//! Layout is in content pixels from the document's top-left corner; the view
//! shows the part at (scroll_x, scroll_y).

use std::collections::HashMap;

use crate::document::{Document, PageBitmap};
use crate::overlay::{Kind, Overlay};
use crate::theme::{self, Metrics};

#[derive(Clone, Copy)]
pub struct PageRect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

#[derive(Default)]
struct Layout {
    key: (i32, u32, u32), // (editor width, dpi, zoom bits) it was computed for
    pages: Vec<PageRect>,
    total_h: i32,
    total_w: i32,
    max_w: i32,
}

/// Zoom = width of the widest page as a fraction of the page view's width.
pub const ZOOM_DEFAULT: f32 = 0.75;
pub const ZOOM_MIN: f32 = 0.10;
pub const ZOOM_MAX: f32 = 2.50;
/// Pages bigger than this many pixels are rendered only around what's on
/// screen, so high zoom doesn't need hundreds of MB per page.
const WHOLE_PAGE_PIXELS: i64 = 8_000_000;
/// Undo steps kept per tab.
const UNDO_LIMIT: usize = 200;

/// A change to the page list. Applying one returns the change that undoes
/// it, so history holds small operations instead of copies of the file.
pub enum PageOp {
    /// Move page `from` so it ends up at index `to`.
    Move { from: usize, to: usize },
    /// Insert the only page of `page` at index `at`.
    Insert { at: usize, page: Document },
    Delete { at: usize },
    /// Quarter turns clockwise (negative: counterclockwise).
    Rotate { page: usize, quarters: i32 },
}

/// One undo (or redo) step: the overlays to go back to, and the page change
/// that undoes the page edit, if there was one.
struct Step {
    overlays: Vec<Overlay>,
    page: Option<PageOp>,
}

pub struct Tab {
    pub doc: Document,
    /// Shown as "Untitled-N" until first saved.
    pub untitled: Option<u32>,
    pub scroll_y: i32,
    pub scroll_x: i32,
    pub zoom: f32,
    /// Edited since the last save (shown as ● on the tab).
    pub dirty: bool,
    /// Text, highlights and signatures placed on the pages.
    pub overlays: Vec<Overlay>,
    pub selected: Option<usize>,
    /// While typing into the selected text overlay: caret byte offset.
    pub caret: Option<usize>,
    undo: Vec<Step>,
    redo: Vec<Step>,
    /// Overlays as they were when the change in progress (a drag, a typing
    /// session) began; recorded as one undo step when it ends.
    pending: Option<Vec<Overlay>>,
    /// Bumped whenever the pages change (search results go stale).
    pub generation: u64,
    layout: Layout,
    pages: HashMap<usize, PageBitmap>,
    thumbs: HashMap<usize, PageBitmap>,
}

impl Tab {
    pub fn new(doc: Document, untitled: Option<u32>) -> Self {
        Self {
            doc,
            untitled,
            scroll_y: 0,
            scroll_x: 0,
            zoom: ZOOM_DEFAULT,
            dirty: false,
            overlays: Vec::new(),
            selected: None,
            caret: None,
            undo: Vec::new(),
            redo: Vec::new(),
            pending: None,
            generation: 0,
            layout: Layout::default(),
            pages: HashMap::new(),
            thumbs: HashMap::new(),
        }
    }

    pub fn name(&self) -> String {
        match (self.doc.file_name(), self.untitled) {
            (Some(name), _) => name,
            (None, Some(n)) => format!("Untitled-{n}"),
            (None, None) => "Untitled".into(),
        }
    }

    /// Call after the page list changes: pages are re-laid out and re-rendered.
    pub fn pages_changed(&mut self) {
        self.dirty = true;
        self.generation += 1;
        self.layout = Layout::default();
        self.pages.clear();
        self.thumbs.clear();
    }

    /// After a save: the file was reloaded, so overlays saved as stamps come
    /// back from it as pages render again.
    /// History is dropped too: the reloaded file now holds the overlays, and
    /// older steps would bring back copies of them.
    pub fn reloaded(&mut self) {
        self.overlays.clear();
        self.selected = None;
        self.caret = None;
        self.undo.clear();
        self.redo.clear();
        self.pending = None;
        self.pages.clear();
        self.thumbs.clear();
    }

    // ---- undo ----------------------------------------------------------------

    fn push_undo(&mut self, step: Step) {
        self.undo.push(step);
        if self.undo.len() > UNDO_LIMIT {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    /// Marks the start of an overlay change (call before changing them).
    pub fn begin_change(&mut self) {
        if self.pending.is_none() {
            self.pending = Some(self.overlays.clone());
        }
    }

    /// Records the change since `begin_change` as one undo step, if anything changed.
    pub fn end_change(&mut self) {
        if let Some(before) = self.pending.take() {
            if before != self.overlays {
                self.dirty = true;
                self.push_undo(Step { overlays: before, page: None });
            }
        }
    }

    /// An overlay change made in one go.
    pub fn change(&mut self, f: impl FnOnce(&mut Self)) {
        self.begin_change();
        f(self);
        self.end_change();
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Undoes (or with `redo`, redoes) one step. Ok(false) if there was none.
    pub fn undo(&mut self, redo: bool) -> Result<bool, String> {
        self.end_edit(); // a typing session in progress becomes its own step first
        self.end_change();
        let Some(step) = (if redo { self.redo.pop() } else { self.undo.pop() }) else { return Ok(false) };
        let had_page_op = step.page.is_some();
        let back_page = match step.page {
            Some(op) => match self.apply(op) {
                Ok(inverse) => Some(inverse),
                Err(e) => return Err(e),
            },
            None => None,
        };
        let now = std::mem::replace(&mut self.overlays, step.overlays);
        let back = Step { overlays: now, page: back_page };
        if redo { self.undo.push(back) } else { self.redo.push(back) }
        self.selected = None;
        self.caret = None;
        if had_page_op {
            self.pages_changed();
        }
        self.dirty = true;
        Ok(true)
    }

    // ---- page edits ------------------------------------------------------------

    /// Applies a page change to the document only; returns its inverse.
    fn apply(&mut self, op: PageOp) -> Result<PageOp, String> {
        Ok(match op {
            PageOp::Move { from, to } => {
                self.doc.move_page(from, to)?;
                PageOp::Move { from: to, to: from }
            }
            PageOp::Insert { at, page } => {
                self.doc.import_page(&page, 0, at)?;
                PageOp::Delete { at }
            }
            PageOp::Delete { at } => {
                let page = Document::from_page(&self.doc, at)?;
                self.doc.delete_page(at);
                PageOp::Insert { at, page }
            }
            PageOp::Rotate { page, quarters } => {
                self.doc.rotate_page(page, quarters)?;
                PageOp::Rotate { page, quarters: -quarters }
            }
        })
    }

    /// Makes a page edit the user asked for: changes the document, carries
    /// the overlays along, and records it for undo.
    pub fn page_edit(&mut self, op: PageOp) -> Result<(), String> {
        self.end_edit();
        self.end_change();
        let before = self.overlays.clone();
        let size = match &op {
            PageOp::Rotate { page, .. } => self.doc.page_sizes.get(*page).copied(),
            _ => None,
        };
        let follow: Box<dyn Fn(&mut Overlay) -> bool> = match &op {
            &PageOp::Move { from, to } => Box::new(move |o| {
                o.page = match o.page {
                    p if p == from => to,
                    p if from < to && p > from && p <= to => p - 1,
                    p if to < from && p >= to && p < from => p + 1,
                    p => p,
                };
                true
            }),
            &PageOp::Insert { at, .. } => Box::new(move |o| {
                if o.page >= at {
                    o.page += 1;
                }
                true
            }),
            &PageOp::Delete { at } => Box::new(move |o| {
                if o.page > at {
                    o.page -= 1;
                }
                o.page != at
            }),
            &PageOp::Rotate { page, quarters } => Box::new(move |o| {
                if o.page == page {
                    rotate_overlay(o, size.unwrap_or((612.0, 792.0)), quarters);
                }
                true
            }),
        };
        let inverse = self.apply(op)?;
        self.overlays.retain_mut(|o| follow(o));
        self.selected = None;
        self.push_undo(Step { overlays: before, page: Some(inverse) });
        self.pages_changed();
        Ok(())
    }

    // ---- overlays ----------------------------------------------------------

    /// Starts typing into text overlay `i`. `base` is how the overlays looked
    /// before (without the box, if it was just made) for undo.
    pub fn start_edit(&mut self, base: Vec<Overlay>, i: usize, caret: usize) {
        self.end_edit();
        self.end_change();
        self.pending = Some(base);
        self.selected = Some(i);
        self.caret = Some(caret);
    }

    /// Stops typing; a text box left empty is removed. The session becomes
    /// one undo step.
    pub fn end_edit(&mut self) {
        if self.caret.take().is_none() {
            return;
        }
        if let Some(i) = self.selected {
            if matches!(&self.overlays[i].kind, Kind::Text { text, .. } if text.trim().is_empty()) {
                self.overlays.remove(i);
                self.selected = None;
            }
        }
        self.end_change();
    }

    /// Runs `f` on the text being edited and its caret (byte offset). `f`
    /// returns whether it changed the text; the box is refitted if so.
    /// Returns false when nothing is being edited.
    pub fn edit_text(&mut self, f: impl FnOnce(&mut String, &mut usize) -> bool) -> bool {
        let (Some(caret), Some(i)) = (self.caret.as_mut(), self.selected) else { return false };
        let Some(o) = self.overlays.get_mut(i) else { return false };
        let Some(text) = o.text_mut() else { return false };
        if f(text, caret) {
            o.fit_text();
            self.dirty = true;
        }
        true
    }

    pub fn remove_overlay(&mut self, i: usize) {
        if i < self.overlays.len() {
            self.end_edit();
            self.change(|t| {
                t.overlays.remove(i);
            });
            self.selected = None;
        }
    }

    /// Copies of the overlays on page `page`.
    pub fn overlays_on(&self, page: usize) -> Vec<Overlay> {
        self.overlays.iter().filter(|o| o.page == page).cloned().collect()
    }

    /// Adds copies of overlays onto page `page` (part of the page edit just
    /// made, so not a separate undo step).
    pub fn add_overlays(&mut self, copies: Vec<Overlay>, page: usize) {
        self.overlays.extend(copies.into_iter().map(|o| Overlay { page, ..o }));
    }

    /// A thumbnail already rendered (for the drag ghost); never renders.
    pub fn cached_thumb(&self, i: usize) -> Option<&PageBitmap> {
        self.thumbs.get(&i)
    }

    /// Pages stacked vertically, the widest `zoom` × the view's width, each
    /// centered. Wider than the view, the document scrolls sideways.
    pub fn ensure_layout(&mut self, editor_w: i32, m: Metrics) {
        let key = (editor_w, m.dpi, self.zoom.to_bits());
        if self.layout.key == key && !self.layout.pages.is_empty() {
            return;
        }
        let margin = m.px(theme::PAGE_MARGIN);
        let gap = m.px(theme::PAGE_GAP);
        let widest = self.doc.page_sizes.iter().map(|s| s.0).fold(1.0f32, f32::max);
        let scale = (editor_w as f32 * self.zoom).max(1.0) / widest;
        let max_w = ((widest * scale).round() as i32).max(1);
        let total_w = editor_w.max(max_w + 2 * margin);

        let mut y = margin;
        let pages = self
            .doc
            .page_sizes
            .iter()
            .map(|&(pw, ph)| {
                let w = (pw * scale).round().max(1.0) as i32;
                let h = (ph * scale).round().max(1.0) as i32;
                let r = PageRect { x: (total_w - w) / 2, y, w, h };
                y += h + gap;
                r
            })
            .collect();
        self.layout = Layout { key, pages, total_h: y - gap + margin, total_w, max_w };
    }

    pub fn pages(&self) -> &[PageRect] {
        &self.layout.pages
    }

    pub fn content_h(&self) -> i32 {
        self.layout.total_h
    }

    pub fn max_page_w(&self) -> i32 {
        self.layout.max_w
    }

    pub fn content_w(&self) -> i32 {
        self.layout.total_w
    }

    pub fn max_scroll(&self, view_h: i32) -> i32 {
        (self.layout.total_h - view_h).max(0)
    }

    pub fn max_scroll_x(&self, view_w: i32) -> i32 {
        (self.layout.total_w - view_w).max(0)
    }

    /// Clamps and sets the horizontal scroll offset; returns true if it changed.
    pub fn scroll_x_to(&mut self, x: i32, view_w: i32) -> bool {
        let x = x.clamp(0, self.max_scroll_x(view_w));
        let changed = x != self.scroll_x;
        self.scroll_x = x;
        changed
    }

    /// Clamps and sets the scroll offset; returns true if it changed.
    pub fn scroll_to(&mut self, y: i32, view_h: i32) -> bool {
        let y = y.clamp(0, self.max_scroll(view_h));
        let changed = y != self.scroll_y;
        self.scroll_y = y;
        changed
    }

    /// Index of the page under the middle of the viewport.
    pub fn current_page(&self, view_h: i32) -> usize {
        let mid = self.scroll_y + view_h / 2;
        self.layout
            .pages
            .iter()
            .position(|p| p.y + p.h >= mid)
            .unwrap_or(self.layout.pages.len().saturating_sub(1))
    }

    /// Indices of pages intersecting content rows [top, bottom).
    pub fn pages_in(&self, top: i32, bottom: i32) -> std::ops::Range<usize> {
        let pages = &self.layout.pages;
        let first = pages.partition_point(|p| p.y + p.h <= top);
        let last = pages.partition_point(|p| p.y < bottom);
        first..last.max(first)
    }

    /// The bitmap for page `i` at width `w`, covering at least `visible`
    /// (x0, y0, x1, y1 in that page's pixels). Renders when missing or stale
    /// if `render` is set; otherwise may return a stale bitmap to stretch.
    pub fn page(&mut self, i: usize, w: i32, visible: (i32, i32, i32, i32), render: bool) -> Option<&PageBitmap> {
        let h = self.doc.page_height(i, w);
        let region = if w as i64 * h as i64 <= WHOLE_PAGE_PIXELS {
            None
        } else {
            // A screenful of slack around what's visible, so scrolling a bit
            // doesn't re-render.
            let (x0, y0, x1, y1) = visible;
            let (sw, sh) = (x1 - x0, y1 - y0);
            Some((x0 - sw / 2, y0 - sh, x1 + sw / 2, y1 + sh))
        };
        let bmp = Self::cached(&mut self.doc, &mut self.pages, i, w, visible, region, render);
        self.overlays.append(&mut self.doc.found);
        bmp
    }

    pub fn thumb(&mut self, i: usize, w: i32, render: bool) -> Option<&PageBitmap> {
        let all = (0, 0, w, self.doc.page_height(i, w));
        let bmp = Self::cached(&mut self.doc, &mut self.thumbs, i, w, all, None, render);
        self.overlays.append(&mut self.doc.found);
        bmp
    }

    #[allow(clippy::too_many_arguments)]
    fn cached<'a>(
        doc: &mut Document,
        cache: &'a mut HashMap<usize, PageBitmap>,
        i: usize,
        w: i32,
        need: (i32, i32, i32, i32),
        region: Option<(i32, i32, i32, i32)>,
        render: bool,
    ) -> Option<&'a PageBitmap> {
        let stale = cache.get(&i).is_none_or(|b| !b.covers(w, need));
        if stale && render {
            if let Ok(bmp) = doc.render(i, w, region) {
                cache.insert(i, bmp);
            }
        }
        cache.get(&i)
    }

    /// Drops bitmaps outside the given ranges (plus slack) to keep memory flat.
    pub fn evict(&mut self, pages: std::ops::Range<usize>, thumbs: std::ops::Range<usize>) {
        const SLACK: usize = 2;
        let keep = |r: &std::ops::Range<usize>, i: usize| i + SLACK >= r.start && i < r.end + SLACK;
        self.pages.retain(|&i, _| keep(&pages, i));
        self.thumbs.retain(|&i, _| keep(&thumbs, i));
    }
}

/// Moves an overlay with its page turning `quarters` quarter turns clockwise.
/// `(pw, ph)` is the page's displayed size before turning. Overlays stay
/// upright; marks turn with the area they cover.
fn rotate_overlay(o: &mut Overlay, (mut pw, mut ph): (f32, f32), quarters: i32) {
    for _ in 0..quarters.rem_euclid(4) {
        let (cx, cy) = (o.x + o.w / 2.0, o.y + o.h / 2.0);
        let (ncx, ncy) = (ph - cy, cx);
        if matches!(o.kind, Kind::Mark(_)) {
            (o.w, o.h) = (o.h, o.w);
        }
        (o.x, o.y) = (ncx - o.w / 2.0, ncy - o.h / 2.0);
        (pw, ph) = (ph, pw);
    }
    let _ = pw;
}

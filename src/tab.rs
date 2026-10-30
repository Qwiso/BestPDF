//! One open document: its scroll position, page layout and rendered bitmaps,
//! the overlays placed on it, and its undo history.
//! Layout is in content pixels from the document's top-left corner; the view
//! shows the part at (scroll_x, scroll_y).

use std::collections::{BTreeSet, HashMap};

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
    /// (view width, view height, dpi, zoom bits, fit, spread) it was computed for.
    key: (i32, i32, u32, u32, Fit, bool),
    pages: Vec<PageRect>,
    /// Bottom of each page's row (pages side by side can differ in height).
    row_bottom: Vec<i32>,
    total_h: i32,
    total_w: i32,
    /// Width of the widest row, and where rows start (centered in total_w).
    max_w: i32,
    left: i32,
}

/// Zoom = width of the widest row of pages as a fraction of the page view's width.
pub const ZOOM_DEFAULT: f32 = 0.75;
pub const ZOOM_MIN: f32 = 0.10;
pub const ZOOM_MAX: f32 = 2.50;
/// Pages bigger than this many pixels are rendered only around what's on
/// screen, so high zoom doesn't need hundreds of MB per page.
const WHOLE_PAGE_PIXELS: i64 = 8_000_000;
/// Undo steps kept per tab.
const UNDO_LIMIT: usize = 200;

/// How the zoom is chosen: set by the user, or kept fitting the view as the
/// window changes.
#[derive(Clone, Copy, PartialEq, Default, Debug)]
pub enum Fit {
    #[default]
    Zoom,
    Width,
    Page,
}

/// A change to the page list. Applying one returns the change that undoes
/// it, so history holds small operations instead of copies of the file.
pub enum PageOp {
    /// Move page `from` so it ends up at index `to`.
    Move { from: usize, to: usize },
    /// Insert all pages of `pages` at index `at`.
    Insert { at: usize, pages: Document },
    /// Delete `count` pages starting at `at`.
    Delete { at: usize, count: usize },
    /// Quarter turns clockwise (negative: counterclockwise).
    Rotate { page: usize, quarters: i32 },
    /// Several changes as one step, in order.
    Batch(Vec<PageOp>),
}

/// One undo (or redo) step: the overlays to go back to, and the page change
/// that undoes the page edit, if there was one. `id` names the state the
/// step leads away from, for telling whether the file is saved.
struct Step {
    id: u64,
    overlays: Vec<Overlay>,
    page: Option<PageOp>,
}

/// Where a file was left: restored when it's opened again.
#[derive(Clone, Copy)]
pub struct ViewState {
    /// Page at the top of the view and how far down it the view starts (0..1).
    pub page: usize,
    pub frac: f32,
    pub zoom: f32,
    pub fit: Fit,
    pub spread: bool,
}

pub struct Tab {
    pub doc: Document,
    /// Shown as "Untitled-N" until first saved.
    pub untitled: Option<u32>,
    pub scroll_y: i32,
    pub scroll_x: i32,
    pub zoom: f32,
    pub fit: Fit,
    /// Two pages side by side.
    pub spread: bool,
    /// Never saved, though made from other pages (counts as unsaved).
    pub unsaved: bool,
    /// Text, highlights and signatures placed on the pages.
    pub overlays: Vec<Overlay>,
    pub selected: Option<usize>,
    /// While typing into the selected text overlay: caret byte offset.
    pub caret: Option<usize>,
    /// Pages selected in the minimap, and where a Shift+click range starts.
    pub page_selection: BTreeSet<usize>,
    pub selection_anchor: Option<usize>,
    undo: Vec<Step>,
    redo: Vec<Step>,
    next_id: u64,
    /// `id` of the top undo step when last saved (None: history was empty).
    saved_at: Option<u64>,
    /// Overlays as they were when the change in progress (a drag, a typing
    /// session) began; recorded as one undo step when it ends.
    pending: Option<Vec<Overlay>>,
    /// Bumped whenever the pages change (search results go stale).
    pub generation: u64,
    /// Where to scroll once the layout exists (reopened file).
    restore: Option<ViewState>,
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
            fit: Fit::Zoom,
            spread: false,
            unsaved: false,
            overlays: Vec::new(),
            selected: None,
            caret: None,
            page_selection: BTreeSet::new(),
            selection_anchor: None,
            undo: Vec::new(),
            redo: Vec::new(),
            next_id: 1,
            saved_at: None,
            pending: None,
            generation: 0,
            restore: None,
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

    /// Edited since the last save (shown as ● on the tab). Undoing back to
    /// the saved state counts as clean again.
    pub fn is_dirty(&self) -> bool {
        self.unsaved || self.pending.as_ref().is_some_and(|p| *p != self.overlays) || self.undo.last().map(|s| s.id) != self.saved_at
    }

    /// After a save that kept the overlays editable.
    pub fn mark_saved(&mut self) {
        self.unsaved = false;
        self.saved_at = self.undo.last().map(|s| s.id);
    }

    /// Call after the page list changes: pages are re-laid out and re-rendered.
    pub fn pages_changed(&mut self) {
        self.generation += 1;
        self.page_selection.clear();
        self.selection_anchor = None;
        self.layout = Layout::default();
        self.pages.clear();
        self.thumbs.clear();
    }

    /// After a flattening save: the overlays are now part of the pages, so
    /// they and the history (which would bring them back) are dropped.
    pub fn flattened(&mut self) {
        self.overlays.clear();
        self.selected = None;
        self.caret = None;
        self.undo.clear();
        self.redo.clear();
        self.pending = None;
        self.saved_at = None;
        self.unsaved = false;
        self.pages.clear();
        self.thumbs.clear();
    }

    // ---- view ------------------------------------------------------------------

    /// Where the view is, for reopening the file later.
    pub fn view_state(&self) -> ViewState {
        let pages = &self.layout.pages;
        let page = pages.iter().position(|p| p.y + p.h > self.scroll_y).unwrap_or(0);
        let frac = pages.get(page).map_or(0.0, |p| ((self.scroll_y - p.y) as f32 / p.h.max(1) as f32).clamp(0.0, 1.0));
        ViewState { page, frac, zoom: self.zoom, fit: self.fit, spread: self.spread }
    }

    /// Returns to a saved view once the layout is ready.
    pub fn restore_view(&mut self, v: ViewState) {
        (self.zoom, self.fit, self.spread) = (v.zoom.clamp(ZOOM_MIN, ZOOM_MAX), v.fit, v.spread);
        self.restore = Some(v);
    }

    // ---- undo ----------------------------------------------------------------

    fn push_undo(&mut self, overlays: Vec<Overlay>, page: Option<PageOp>) {
        let id = self.next_id;
        self.next_id += 1;
        self.undo.push(Step { id, overlays, page });
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
                self.push_undo(before, None);
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
        !self.undo.is_empty() || self.caret.is_some()
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
            Some(op) => Some(self.apply(op, false)?),
            None => None,
        };
        let now = std::mem::replace(&mut self.overlays, step.overlays);
        let back = Step { id: step.id, overlays: now, page: back_page };
        if redo { self.undo.push(back) } else { self.redo.push(back) }
        self.selected = None;
        self.caret = None;
        if had_page_op {
            self.pages_changed();
        }
        Ok(true)
    }

    // ---- page edits ------------------------------------------------------------

    /// Applies a page change to the document, and with `follow`, moves the
    /// overlays along (undo restores them from its snapshot instead).
    /// Returns the change that undoes it.
    fn apply(&mut self, op: PageOp, follow: bool) -> Result<PageOp, String> {
        let shift = |overlays: &mut Vec<Overlay>, f: &dyn Fn(usize) -> Option<usize>| {
            overlays.retain_mut(|o| match f(o.page) {
                Some(p) => {
                    o.page = p;
                    true
                }
                None => false,
            })
        };
        Ok(match op {
            PageOp::Move { from, to } => {
                self.doc.move_page(from, to)?;
                if follow {
                    shift(&mut self.overlays, &|p| {
                        Some(match p {
                            _ if p == from => to,
                            _ if from < to && p > from && p <= to => p - 1,
                            _ if to < from && p >= to && p < from => p + 1,
                            _ => p,
                        })
                    });
                }
                PageOp::Move { from: to, to: from }
            }
            PageOp::Insert { at, pages } => {
                let n = pages.page_count();
                self.doc.import_pages(&pages, None, at)?;
                if follow {
                    shift(&mut self.overlays, &|p| Some(if p >= at { p + n } else { p }));
                }
                PageOp::Delete { at, count: n }
            }
            PageOp::Delete { at, count } => {
                let pages = Document::from_pages(&self.doc, &(at..at + count).collect::<Vec<_>>())?;
                self.doc.delete_pages(at, count);
                if follow {
                    shift(&mut self.overlays, &|p| match p {
                        _ if p < at => Some(p),
                        _ if p < at + count => None,
                        _ => Some(p - count),
                    });
                }
                PageOp::Insert { at, pages }
            }
            PageOp::Rotate { page, quarters } => {
                let size = self.doc.page_sizes.get(page).copied().unwrap_or((612.0, 792.0));
                self.doc.rotate_page(page, quarters)?;
                if follow {
                    for o in self.overlays.iter_mut().filter(|o| o.page == page) {
                        rotate_overlay(o, size, quarters);
                    }
                }
                PageOp::Rotate { page, quarters: -quarters }
            }
            PageOp::Batch(ops) => {
                let mut inverses = Vec::new();
                for op in ops {
                    match self.apply(op, follow) {
                        Ok(inverse) => inverses.push(inverse),
                        Err(e) => {
                            // Put back what was done so far.
                            for inverse in inverses.into_iter().rev() {
                                let _ = self.apply(inverse, follow);
                            }
                            return Err(e);
                        }
                    }
                }
                inverses.reverse();
                PageOp::Batch(inverses)
            }
        })
    }

    /// Makes a page edit the user asked for: changes the document, carries
    /// the overlays along, and records it for undo as one step.
    pub fn page_edit(&mut self, op: PageOp) -> Result<(), String> {
        self.end_edit();
        self.end_change();
        let before = self.overlays.clone();
        let inverse = self.apply(op, true)?;
        self.selected = None;
        self.push_undo(before, Some(inverse));
        self.pages_changed();
        Ok(())
    }

    /// Moves `pages` (in order) so they sit together before index `at`.
    pub fn move_pages(&mut self, pages: &[usize], at: usize) -> Result<(), String> {
        let sel: Vec<usize> = pages.iter().copied().collect::<BTreeSet<_>>().into_iter().collect();
        let n = self.doc.page_count();
        let target = at.min(n) - sel.iter().filter(|&&p| p < at).count();
        // The order wanted: the other pages, with the moved ones together at `target`.
        let mut want: Vec<usize> = (0..n).filter(|p| !sel.contains(p)).collect();
        want.splice(target..target, sel.iter().copied());
        // Single moves that build it front to back; each fixes one position.
        let mut order: Vec<usize> = (0..n).collect();
        let mut ops = Vec::new();
        for dest in 0..n {
            if order[dest] != want[dest] {
                let cur = order.iter().position(|&p| p == want[dest]).unwrap_or(dest);
                let page = order.remove(cur);
                order.insert(dest, page);
                ops.push(PageOp::Move { from: cur, to: dest });
            }
        }
        if ops.is_empty() {
            return Ok(());
        }
        self.page_edit(PageOp::Batch(ops))
    }

    /// Deletes `pages` as one step.
    pub fn delete_pages(&mut self, pages: &[usize]) -> Result<(), String> {
        // Contiguous runs, last first, so earlier indices stay valid.
        let sel: Vec<usize> = pages.iter().copied().collect::<BTreeSet<_>>().into_iter().collect();
        let mut runs: Vec<(usize, usize)> = Vec::new();
        for p in sel {
            match runs.last_mut() {
                Some((at, count)) if *at + *count == p => *count += 1,
                _ => runs.push((p, 1)),
            }
        }
        let ops = runs.into_iter().rev().map(|(at, count)| PageOp::Delete { at, count }).collect();
        self.page_edit(PageOp::Batch(ops))
    }

    pub fn rotate_pages(&mut self, pages: &[usize], quarters: i32) -> Result<(), String> {
        let ops = pages.iter().map(|&page| PageOp::Rotate { page, quarters }).collect();
        self.page_edit(PageOp::Batch(ops))
    }

    /// The minimap selection, or `fallback` alone when nothing is selected.
    pub fn selected_pages(&self, fallback: usize) -> Vec<usize> {
        if self.page_selection.is_empty() {
            vec![fallback]
        } else {
            self.page_selection.iter().copied().collect()
        }
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

    // ---- layout ------------------------------------------------------------------

    /// Pages in rows (one, or two side by side), each row centered. The
    /// widest row is `zoom` × the view's width, or fits the view (Fit). Wider
    /// than the view, the document scrolls sideways.
    pub fn ensure_layout(&mut self, view_w: i32, view_h: i32, m: Metrics) {
        let key = (view_w, view_h, m.dpi, self.zoom.to_bits(), self.fit, self.spread);
        if self.layout.key == key && !self.layout.pages.is_empty() {
            return;
        }
        let margin = m.px(theme::PAGE_MARGIN);
        let gap = m.px(theme::PAGE_GAP);
        let per_row = if self.spread { 2 } else { 1 };
        let sizes = &self.doc.page_sizes;
        let rows: Vec<std::ops::Range<usize>> = (0..sizes.len()).step_by(per_row).map(|i| i..(i + per_row).min(sizes.len())).collect();
        // Row sizes in points (the gap between pages is in pixels, added after scaling).
        let row_w_pt = |r: &std::ops::Range<usize>| sizes[r.clone()].iter().map(|s| s.0).sum::<f32>();
        let row_h_pt = |r: &std::ops::Range<usize>| sizes[r.clone()].iter().map(|s| s.1).fold(1.0f32, f32::max);
        let widest = rows.iter().map(row_w_pt).fold(1.0f32, f32::max);
        let tallest = rows.iter().map(row_h_pt).fold(1.0f32, f32::max);
        let inner_gap = gap * (per_row as i32 - 1);
        let fit_w = ((view_w - 2 * margin - inner_gap) as f32).max(10.0) / widest;
        let scale = match self.fit {
            Fit::Zoom => (view_w as f32 * self.zoom).max(1.0) / widest,
            Fit::Width => fit_w,
            Fit::Page => fit_w.min(((view_h - 2 * margin) as f32).max(10.0) / tallest),
        };
        if self.fit != Fit::Zoom {
            self.zoom = (scale * widest / view_w.max(1) as f32).clamp(ZOOM_MIN, ZOOM_MAX);
        }
        let px = |v: f32| (v * scale).round().max(1.0) as i32;
        let max_w = rows.iter().map(|r| sizes[r.clone()].iter().map(|s| px(s.0)).sum::<i32>() + gap * (r.len() as i32 - 1)).max().unwrap_or(1);
        let total_w = view_w.max(max_w + 2 * margin);

        let mut y = margin;
        let (mut pages, mut row_bottom) = (Vec::new(), Vec::new());
        for r in &rows {
            let row_w = sizes[r.clone()].iter().map(|s| px(s.0)).sum::<i32>() + gap * (r.len() as i32 - 1);
            let row_h = sizes[r.clone()].iter().map(|s| px(s.1)).max().unwrap_or(1);
            let mut x = (total_w - row_w) / 2;
            for &(pw, ph) in &sizes[r.clone()] {
                pages.push(PageRect { x, y, w: px(pw), h: px(ph) });
                row_bottom.push(y + row_h);
                x += px(pw) + gap;
            }
            y += row_h + gap;
        }
        self.layout = Layout { key, pages, row_bottom, total_h: y - gap + margin, total_w, max_w, left: (total_w - max_w) / 2 };

        if let Some(v) = self.restore.take() {
            if let Some(p) = self.layout.pages.get(v.page) {
                self.scroll_y = p.y + (v.frac * p.h as f32) as i32;
            }
        }
    }

    pub fn pages(&self) -> &[PageRect] {
        &self.layout.pages
    }

    pub fn content_h(&self) -> i32 {
        self.layout.total_h
    }

    /// Width of the widest row of pages.
    pub fn max_page_w(&self) -> i32 {
        self.layout.max_w
    }

    /// Left edge of the widest row.
    pub fn content_left(&self) -> i32 {
        self.layout.left
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
        self.layout.row_bottom.iter().position(|&b| b >= mid).unwrap_or(self.layout.pages.len().saturating_sub(1))
    }

    /// Indices of pages whose rows intersect content rows [top, bottom).
    pub fn pages_in(&self, top: i32, bottom: i32) -> std::ops::Range<usize> {
        let first = self.layout.row_bottom.partition_point(|&b| b <= top);
        let last = self.layout.pages.partition_point(|p| p.y < bottom);
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
fn rotate_overlay(o: &mut Overlay, (pw, ph): (f32, f32), quarters: i32) {
    let mut ph = ph;
    let mut pw = pw;
    for _ in 0..quarters.rem_euclid(4) {
        let (cx, cy) = (o.x + o.w / 2.0, o.y + o.h / 2.0);
        let (ncx, ncy) = (ph - cy, cx);
        if matches!(o.kind, Kind::Mark(_)) {
            (o.w, o.h) = (o.h, o.w);
        }
        (o.x, o.y) = (ncx - o.w / 2.0, ncy - o.h / 2.0);
        std::mem::swap(&mut pw, &mut ph);
    }
}

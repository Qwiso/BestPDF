//! One open document: its scroll position, page layout and rendered bitmaps.
//! Layout is in content pixels: x within the editor, y from the top of the document.

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
    key: (i32, u32), // (editor width, dpi) it was computed for
    pages: Vec<PageRect>,
    total_h: i32,
    max_w: i32,
}

pub struct Tab {
    pub doc: Document,
    /// Shown as "Untitled-N" until first saved.
    pub untitled: Option<u32>,
    pub scroll_y: i32,
    /// Edited since the last save (shown as ● on the tab).
    pub dirty: bool,
    /// Text, highlights and signatures placed on the pages.
    pub overlays: Vec<Overlay>,
    pub selected: Option<usize>,
    /// While typing into the selected text overlay: caret byte offset.
    pub caret: Option<usize>,
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
            dirty: false,
            overlays: Vec::new(),
            selected: None,
            caret: None,
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
        self.layout = Layout::default();
        self.pages.clear();
        self.thumbs.clear();
    }

    /// After a save: the file was reloaded, so overlays saved as stamps come
    /// back from it as pages render again.
    pub fn reloaded(&mut self) {
        self.overlays.clear();
        self.selected = None;
        self.caret = None;
        self.pages.clear();
        self.thumbs.clear();
    }

    // ---- overlays ----------------------------------------------------------

    /// Stops typing; a text box left empty is removed.
    pub fn end_edit(&mut self) {
        if self.caret.take().is_none() {
            return;
        }
        let Some(i) = self.selected else { return };
        if matches!(&self.overlays[i].kind, Kind::Text { text, .. } if text.trim().is_empty()) {
            self.overlays.remove(i);
            self.selected = None;
        }
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
            self.overlays.remove(i);
            self.selected = None;
            self.caret = None;
            self.dirty = true;
        }
    }

    /// Copies of the overlays on page `page`.
    pub fn overlays_on(&self, page: usize) -> Vec<Overlay> {
        self.overlays.iter().filter(|o| o.page == page).cloned().collect()
    }

    /// Re-indexes overlays after the page list changed: `f` maps an old page
    /// index to its new one, or None if the page is gone.
    pub fn remap_overlays(&mut self, f: impl Fn(usize) -> Option<usize>) {
        self.end_edit();
        self.selected = None;
        self.overlays.retain_mut(|o| match f(o.page) {
            Some(p) => {
                o.page = p;
                true
            }
            None => false,
        });
    }

    /// Adds copies of overlays onto page `page`.
    pub fn add_overlays(&mut self, copies: Vec<Overlay>, page: usize) {
        self.overlays.extend(copies.into_iter().map(|o| Overlay { page, ..o }));
    }

    /// A thumbnail already rendered (for the drag ghost); never renders.
    pub fn cached_thumb(&self, i: usize) -> Option<&PageBitmap> {
        self.thumbs.get(&i)
    }

    /// Fit-to-width layout, capped so pages stay readable on wide monitors.
    pub fn ensure_layout(&mut self, editor_w: i32, m: Metrics) {
        if self.layout.key == (editor_w, m.dpi) && !self.layout.pages.is_empty() {
            return;
        }
        let margin = m.px(theme::PAGE_MARGIN);
        let gap = m.px(theme::PAGE_GAP);
        let avail = (editor_w - 2 * margin).clamp(m.px(100), m.px(theme::MAX_PAGE_W));
        let widest = self.doc.page_sizes.iter().map(|s| s.0).fold(1.0f32, f32::max);
        let scale = avail as f32 / widest;

        let mut y = margin;
        let mut max_w = 1;
        let pages = self
            .doc
            .page_sizes
            .iter()
            .map(|&(pw, ph)| {
                let w = (pw * scale).round().max(1.0) as i32;
                let h = (ph * scale).round().max(1.0) as i32;
                let r = PageRect { x: (editor_w - w) / 2, y, w, h };
                y += h + gap;
                max_w = max_w.max(w);
                r
            })
            .collect();
        self.layout = Layout { key: (editor_w, m.dpi), pages, total_h: y - gap + margin, max_w };
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

    pub fn max_scroll(&self, view_h: i32) -> i32 {
        (self.layout.total_h - view_h).max(0)
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

    /// The bitmap for page `i` at width `w`. Renders when missing or stale if
    /// `render` is set; otherwise may return a stale bitmap to stretch.
    pub fn page(&mut self, i: usize, w: i32, render: bool) -> Option<&PageBitmap> {
        let bmp = Self::cached(&mut self.doc, &mut self.pages, i, w, render);
        self.overlays.append(&mut self.doc.found);
        bmp
    }

    pub fn thumb(&mut self, i: usize, w: i32, render: bool) -> Option<&PageBitmap> {
        let bmp = Self::cached(&mut self.doc, &mut self.thumbs, i, w, render);
        self.overlays.append(&mut self.doc.found);
        bmp
    }

    fn cached<'a>(
        doc: &mut Document,
        cache: &'a mut HashMap<usize, PageBitmap>,
        i: usize,
        w: i32,
        render: bool,
    ) -> Option<&'a PageBitmap> {
        let stale = cache.get(&i).is_none_or(|b| b.width != w);
        if stale && render {
            if let Ok(bmp) = doc.render(i, w) {
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

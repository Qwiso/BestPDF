//! Page-level commands of the App: go to page, view modes, page selection,
//! merging and splitting, exporting images, shrinking, and the password
//! prompt. Most open a `Sheet` first; those that need a file or folder hand
//! that to main.rs (Action::Pick*) and continue in `job_*`.

use std::path::{Path, PathBuf};

use super::{Action, App, Slot};
use crate::document::{Document, Export};
use crate::imageio;
use crate::overlay::Overlay;
use crate::settings::{self, OverlaySave};
use crate::sheet::{self, Field, Sheet, SheetEvent};
use crate::tab::{Fit, PageOp, Tab};

/// What the open sheet is for.
pub enum SheetKind {
    GoTo,
    Extract,
    Split,
    ExportImages,
    Shrink,
    Password { path: PathBuf },
}

/// Work waiting for a file or folder from a dialog.
pub enum Job {
    Insert { at: usize },
    Combine,
    Split { parts: Vec<Vec<usize>> },
    ExportImages { pages: Vec<usize>, format: imageio::Format, dpi: f32 },
    Shrink { dpi: f32, quality: f32 },
}

const DPI_CHOICES: [f32; 3] = [72.0, 150.0, 300.0];
const SHRINK_DPI: [f32; 3] = [96.0, 150.0, 200.0];
const QUALITY: [f32; 3] = [0.5, 0.7, 0.85];

/// Overlays on `pages`, renumbered to their positions in that list.
fn overlays_for(tab: &Tab, pages: &[usize]) -> Vec<Overlay> {
    tab.overlays
        .iter()
        .filter_map(|o| pages.iter().position(|&p| p == o.page).map(|k| Overlay { page: k, ..o.clone() }))
        .collect()
}

fn stem(tab: &Tab) -> String {
    let name = tab.name();
    name.strip_suffix(".pdf").or_else(|| name.strip_suffix(".PDF")).unwrap_or(&name).to_string()
}

impl App {
    // ---- sheets ----------------------------------------------------------------

    fn show_sheet(&mut self, kind: SheetKind, sheet: Sheet) -> Action {
        self.sheet = Some((kind, sheet));
        Action::Redraw
    }

    /// Pages to act on: the minimap selection if any (or if `page` is in it),
    /// else `page`, else the page in view.
    pub(super) fn target_pages(&self, page: Option<usize>) -> Vec<usize> {
        let Some(tab) = self.active_tab() else { return Vec::new() };
        let current = page.unwrap_or_else(|| tab.current_page(self.view_h()));
        match page {
            Some(p) if !tab.page_selection.contains(&p) => vec![p],
            _ => tab.selected_pages(current),
        }
    }

    pub fn open_goto(&mut self) -> Action {
        let Some(tab) = self.active_tab() else { return Action::None };
        let n = tab.doc.page_count();
        let field = Field::Text { label: "Page", value: String::new(), hint: "1", secret: false };
        let sheet = Sheet::new("Go to Page", vec![field], "Go").with_message(format!("This document has {n} pages."), false);
        self.show_sheet(SheetKind::GoTo, sheet)
    }

    pub fn open_extract(&mut self, page: Option<usize>) -> Action {
        if self.active_tab().is_none() {
            return Action::None;
        }
        let pages = sheet::format_pages(&self.target_pages(page));
        let fields = vec![
            Field::Text { label: "Pages", value: pages, hint: "e.g. 1-3, 7", secret: false },
            Field::Check { label: "Remove them from this document", on: false },
        ];
        let sheet = Sheet::new("Extract Pages", fields, "Extract").with_message("They open as a new document.", false);
        self.show_sheet(SheetKind::Extract, sheet)
    }

    pub fn open_split(&mut self) -> Action {
        if self.active_tab().is_none() {
            return Action::None;
        }
        let fields = vec![
            Field::Choice { label: "Split", options: vec!["Every N pages", "By ranges"], index: 0 },
            Field::Text { label: "Pages per file", value: "1".into(), hint: "N", secret: false },
            Field::Text { label: "Ranges", value: String::new(), hint: "e.g. 1-3; 4-10; 11-", secret: false },
        ];
        let sheet = Sheet::new("Split into Files", fields, "Choose Folder…").with_message("Each part is saved as its own PDF.", false);
        self.show_sheet(SheetKind::Split, sheet)
    }

    pub fn open_export_images(&mut self) -> Action {
        if self.active_tab().is_none() {
            return Action::None;
        }
        let pages = match self.active_tab() {
            Some(t) if !t.page_selection.is_empty() => sheet::format_pages(&t.page_selection.iter().copied().collect::<Vec<_>>()),
            _ => "all".into(),
        };
        let fields = vec![
            Field::Text { label: "Pages", value: pages, hint: "all, or e.g. 1-3, 7", secret: false },
            Field::Choice { label: "Format", options: vec!["PNG", "JPEG"], index: 0 },
            Field::Choice { label: "Resolution", options: vec!["72 dpi", "150 dpi", "300 dpi"], index: 1 },
        ];
        self.show_sheet(SheetKind::ExportImages, Sheet::new("Export Pages as Images", fields, "Choose Folder…"))
    }

    pub fn open_shrink(&mut self) -> Action {
        if self.active_tab().is_none() {
            return Action::None;
        }
        let fields = vec![
            Field::Choice { label: "Pictures up to", options: vec!["96 dpi", "150 dpi", "200 dpi"], index: 1 },
            Field::Choice { label: "JPEG quality", options: vec!["Low", "Medium", "High"], index: 1 },
        ];
        let sheet = Sheet::new("Save Reduced-Size Copy", fields, "Save As…")
            .with_message("Large pictures are scaled down and saved as JPEG.", false);
        self.show_sheet(SheetKind::Shrink, sheet)
    }

    /// Asks for the password of `path` (again, if the last one was `wrong`).
    pub fn ask_password(&mut self, path: PathBuf, wrong: bool) {
        let name = path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned());
        let field = Field::Text { label: "Password", value: String::new(), hint: "", secret: true };
        let message = if wrong { "That password didn't work. Try again." } else { "This PDF is protected." };
        let sheet = Sheet::new(format!("Open {name}"), vec![field], "Open").with_message(message, wrong);
        self.sheet = Some((SheetKind::Password { path }, sheet));
    }

    pub(super) fn sheet_event(&mut self, event: SheetEvent) -> Action {
        match event {
            SheetEvent::None => Action::None,
            SheetEvent::Redraw => Action::Redraw,
            SheetEvent::Cancel => {
                self.sheet = None;
                Action::Redraw
            }
            SheetEvent::Ok => {
                let Some((kind, sheet)) = self.sheet.take() else { return Action::None };
                match self.sheet_ok(&kind, &sheet) {
                    Ok(action) => action,
                    Err(e) => {
                        // Keep the sheet open with the problem shown.
                        self.sheet = Some((kind, sheet.with_message(e, true)));
                        Action::Redraw
                    }
                }
            }
        }
    }

    fn sheet_ok(&mut self, kind: &SheetKind, sheet: &Sheet) -> Result<Action, String> {
        let count = self.active_tab().map_or(0, |t| t.doc.page_count());
        Ok(match kind {
            SheetKind::GoTo => {
                let n: usize = sheet.text(0).trim().parse().map_err(|_| "Type a page number.".to_string())?;
                if n == 0 || n > count {
                    return Err(format!("Pages are 1–{count}."));
                }
                self.go_to_page(n - 1)
            }
            SheetKind::Extract => {
                let pages = sheet::parse_pages(sheet.text(0), count)?;
                self.extract_pages(&pages, sheet.checked(1))?;
                Action::Redraw
            }
            SheetKind::Split => {
                let parts = if sheet.choice(0) == 0 {
                    let n: usize = sheet.text(1).trim().parse().ok().filter(|&n| n > 0).ok_or("Pages per file must be a number above 0.")?;
                    (0..count).step_by(n).map(|s| (s..(s + n).min(count)).collect()).collect()
                } else {
                    sheet::parse_groups(sheet.text(2), count)?
                };
                if parts.len() < 2 {
                    return Err("That makes only one file. Pick smaller parts.".into());
                }
                self.job = Some(Job::Split { parts });
                Action::PickFolder("Choose a Folder for the Parts")
            }
            SheetKind::ExportImages => {
                let pages = sheet::parse_pages(sheet.text(0), count)?;
                let format = if sheet.choice(1) == 0 { imageio::Format::Png } else { imageio::Format::Jpeg(0.9) };
                self.job = Some(Job::ExportImages { pages, format, dpi: DPI_CHOICES[sheet.choice(2)] });
                Action::PickFolder("Choose a Folder for the Images")
            }
            SheetKind::Shrink => {
                self.job = Some(Job::Shrink { dpi: SHRINK_DPI[sheet.choice(0)], quality: QUALITY[sheet.choice(1)] });
                let name = self.active_tab().map(stem).unwrap_or_default();
                Action::PickSave(format!("{name} (smaller).pdf"))
            }
            SheetKind::Password { path } => Action::OpenWithPassword(path.clone(), sheet.text(0).to_string()),
        })
    }

    // ---- view ------------------------------------------------------------------

    pub fn go_to_page(&mut self, page: usize) -> Action {
        self.sync_layout();
        let margin = self.m.px(crate::theme::PAGE_MARGIN);
        let Some((y, x)) = self.active_tab().and_then(|t| t.pages().get(page)).map(|p| (p.y, p.x)) else { return Action::None };
        let view_w = self.regions().editor.right - self.regions().editor.left;
        if let Some(tab) = self.active_tab_mut() {
            tab.scroll_x_to(x - margin, view_w);
        }
        self.scroll_to(y - margin);
        Action::Redraw
    }

    pub fn set_fit(&mut self, fit: Fit) -> Action {
        let Some(tab) = self.active_tab_mut() else { return Action::None };
        tab.fit = fit;
        let page = Some(tab.view_state().page);
        self.sync_layout();
        if let Some(p) = page {
            self.go_to_page(p);
        }
        Action::Redraw
    }

    pub fn toggle_spread(&mut self) -> Action {
        let Some(tab) = self.active_tab_mut() else { return Action::None };
        tab.spread = !tab.spread;
        let page = tab.view_state().page;
        tab.pages_changed();
        self.go_to_page(page)
    }

    /// (fit, two pages) of the active tab, for menu check marks.
    pub fn view_mode(&self) -> Option<(Fit, bool)> {
        self.active_tab().map(|t| (t.fit, t.spread))
    }

    /// Saves where every open file is, for next time.
    pub fn remember_views(&self) {
        let views: Vec<(PathBuf, _)> = self
            .tabs
            .iter()
            .filter_map(|s| match s {
                Slot::Doc(t) => Some((t.doc.path.clone()?, t.view_state())),
                _ => None,
            })
            .collect();
        settings::remember_views(&views);
    }

    pub fn select_all_pages(&mut self) -> Action {
        let Some(tab) = self.active_tab_mut() else { return Action::None };
        tab.page_selection = (0..tab.doc.page_count()).collect();
        Action::Redraw
    }

    // ---- merge and split -------------------------------------------------------

    /// Copies `pages` into a new document tab, optionally removing them here.
    fn extract_pages(&mut self, pages: &[usize], remove: bool) -> Result<(), String> {
        let Some(src) = self.active_tab_mut() else { return Ok(()) };
        if remove && pages.len() >= src.doc.page_count() {
            return Err("Can't remove every page. Leave \"Remove\" unticked to copy them all.".into());
        }
        let doc = Document::from_pages(&src.doc, pages)?;
        let copies = overlays_for(src, pages);
        if remove {
            src.delete_pages(pages)?;
        }
        let mut tab = self.make_tab(doc);
        tab.overlays = copies;
        tab.unsaved = true;
        self.tabs.push(Slot::Doc(tab));
        self.activate(self.tabs.len() - 1);
        Ok(())
    }

    pub fn insert_file(&mut self) -> Action {
        let Some(tab) = self.active_tab() else { return Action::None };
        let at = (tab.current_page(self.view_h()) + 1).min(tab.doc.page_count());
        self.job = Some(Job::Insert { at });
        Action::PickOpen("Insert Pages from PDF")
    }

    pub fn combine_files(&mut self) -> Action {
        self.job = Some(Job::Combine);
        Action::PickOpen("Combine PDFs into a New Document")
    }

    /// Files chosen for the pending job (empty: cancelled).
    pub fn job_files(&mut self, paths: Vec<PathBuf>) -> Action {
        let Some(job) = self.job.take() else { return Action::None };
        if paths.is_empty() {
            return Action::None;
        }
        let mut sources = Vec::new();
        for path in &paths {
            match Document::open(path, None) {
                Ok(doc) => sources.push(doc),
                Err(crate::document::OpenError::Password) => {
                    return Action::Error(format!("{} is password-protected. Open it first, then drag its pages across.", path.display()))
                }
                Err(e) => return Action::Error(e.message(path)),
            }
        }
        let result = match job {
            Job::Insert { at } => self.active_tab_mut().map_or(Ok(()), |tab| {
                let ops = sources.into_iter().rev().map(|pages| PageOp::Insert { at, pages }).collect();
                tab.page_edit(PageOp::Batch(ops))
            }),
            Job::Combine => (|| {
                let mut doc = Document::new_empty()?;
                for source in &sources {
                    let at = doc.page_count();
                    doc.import_pages(source, None, at)?;
                }
                let mut tab = self.make_tab(doc);
                tab.unsaved = true;
                self.tabs.push(Slot::Doc(tab));
                self.activate(self.tabs.len() - 1);
                Ok(())
            })(),
            _ => Ok(()),
        };
        match result {
            Ok(()) => Action::Redraw,
            Err(e) => Action::Error(e),
        }
    }

    /// Folder chosen for the pending job (None: cancelled).
    pub fn job_folder(&mut self, folder: Option<PathBuf>) -> Action {
        let (Some(job), Some(folder)) = (self.job.take(), folder) else { return Action::None };
        self.commit_edits();
        let Some(tab) = self.active_tab() else { return Action::None };
        let result = match job {
            Job::Split { parts } => split_into(tab, &parts, &folder),
            Job::ExportImages { pages, format, dpi } => export_images(tab, &pages, format, dpi, &folder),
            _ => Ok(String::new()),
        };
        match result {
            Ok(message) => Action::Info(message),
            Err(e) => Action::Error(e),
        }
    }

    /// File chosen for the pending job (None: cancelled).
    pub fn job_save(&mut self, path: Option<PathBuf>) -> Action {
        let (Some(job), Some(path)) = (self.job.take(), path) else { return Action::None };
        self.commit_edits();
        let Some(tab) = self.active_tab() else { return Action::None };
        let Job::Shrink { dpi, quality } = job else { return Action::None };
        match shrink_copy(tab, dpi, quality, &path) {
            Ok(message) => Action::Info(message),
            Err(e) => Action::Error(e),
        }
    }
}

fn flatten_on_save() -> bool {
    settings::overlay_save() == OverlaySave::Flatten
}

/// Writes each part of `tab` as `<name>-001.pdf`, … in `folder`.
fn split_into(tab: &Tab, parts: &[Vec<usize>], folder: &Path) -> Result<String, String> {
    let name = stem(tab);
    let digits = parts.len().to_string().len().max(2);
    for (k, pages) in parts.iter().enumerate() {
        let part = Document::from_pages(&tab.doc, pages)?;
        let bytes = part.export(&overlays_for(tab, pages), &Export { flatten: flatten_on_save(), plain: false, sid: None })?;
        Document::write_file(&folder.join(format!("{name}-{:0digits$}.pdf", k + 1)), &bytes)?;
    }
    Ok(format!("Saved {} files to {}.", parts.len(), folder.display()))
}

/// Renders `pages` (with their overlays) to picture files in `folder`.
fn export_images(tab: &Tab, pages: &[usize], format: imageio::Format, dpi: f32, folder: &Path) -> Result<String, String> {
    let mut copy = tab.doc.export_doc(&tab.overlays, &Export { flatten: true, plain: true, sid: None })?;
    let name = stem(tab);
    let digits = tab.doc.page_count().to_string().len().max(2);
    for &p in pages {
        let width = ((copy.page_sizes[p].0 / 72.0) * dpi).round() as i32;
        let bmp = copy.render(p, width, None)?;
        // Rendered as BGRx; make it opaque BGRA.
        let mut px = bmp.pixels;
        for c in px.chunks_exact_mut(4) {
            c[3] = 255;
        }
        let file = folder.join(format!("{name}-p{:0digits$}.{}", p + 1, format.extension()));
        imageio::save(&file, bmp.width, bmp.height, &px, format)?;
    }
    copy.found.clear();
    Ok(format!("Saved {} images to {}.", pages.len(), folder.display()))
}

/// Saves a copy with large pictures recompressed.
fn shrink_copy(tab: &Tab, dpi: f32, quality: f32, path: &Path) -> Result<String, String> {
    let before = tab.doc.to_bytes()?.len();
    let mut copy = tab.doc.export_doc(&tab.overlays, &Export { flatten: flatten_on_save(), plain: false, sid: None })?;
    let replaced = copy.shrink_images(dpi, quality);
    let bytes = copy.to_bytes()?;
    Document::write_file(path, &bytes)?;
    let mb = |n: usize| n as f64 / (1024.0 * 1024.0);
    Ok(if replaced == 0 {
        format!("Saved {} ({:.1} MB). No pictures were large enough to shrink.", path.display(), mb(bytes.len()))
    } else {
        format!(
            "Saved {} — {:.1} MB, down from {:.1} MB. {replaced} picture{} recompressed.",
            path.display(),
            mb(bytes.len()),
            mb(before),
            if replaced == 1 { "" } else { "s" }
        )
    })
}

//! Safe wrapper over PDFium. Everything else in the app talks to this module,
//! never to pdfium.rs directly, so the engine stays swappable. (stamp.rs, which
//! turns overlays into PDF objects and back, is the one helper it uses.)

use std::cell::RefCell;
use std::ffi::{c_int, c_ulong, c_void, CString};
use std::fs::File;
use std::io::Write;
use std::os::windows::fs::FileExt;
use std::path::{Path, PathBuf};

use windows::Win32::Graphics::Gdi::HDC;

use crate::imageio;
use crate::overlay::{Image, Overlay};
use crate::pdfium::{self, api, FPDF_DOCUMENT, FPDF_FILEACCESS, FPDF_FILEWRITE, FPDF_PAGE, FPDF_TEXTPAGE, FS_MATRIX, FS_SIZEF};
use crate::stamp;

/// Loads pdfium.dll from next to the executable (portable layout).
pub fn init() -> Result<(), String> {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));
    pdfium::load(&exe_dir)
}

/// A rendered page, or part of one, in 32-bit BGRx, top-down, stride = width * 4.
pub struct PageBitmap {
    pub width: i32,
    pub height: i32,
    pub pixels: Vec<u8>,
    /// Width of the whole page at this scale, and where this bitmap sits in it.
    pub page_w: i32,
    pub x0: i32,
    pub y0: i32,
}

impl PageBitmap {
    /// Covers rows/columns [x0, x1) × [y0, y1) of the page at width `page_w`.
    pub fn covers(&self, page_w: i32, (x0, y0, x1, y1): (i32, i32, i32, i32)) -> bool {
        self.page_w == page_w && self.x0 <= x0 && self.y0 <= y0 && self.x0 + self.width >= x1 && self.y0 + self.height >= y1
    }
}

/// What PDFium reads the document from; must outlive the document handle.
enum Backing {
    None,
    /// Read lazily, block by block. std opens files with delete-sharing, so a
    /// save can rename a new file over this one while it is still open.
    File(#[allow(dead_code)] Box<FileSource>),
    Memory(#[allow(dead_code)] Vec<u8>),
}

#[repr(C)] // `access` must be first: PDFium hands its address back to `read_block`
struct FileSource {
    access: FPDF_FILEACCESS,
    file: File,
}

unsafe extern "C" fn read_block(param: *mut c_void, position: c_ulong, buf: *mut u8, size: c_ulong) -> c_int {
    let source = &*(param as *const FileSource);
    let out = std::slice::from_raw_parts_mut(buf, size as usize);
    let mut done = 0;
    while done < out.len() {
        match source.file.seek_read(&mut out[done..], position as u64 + done as u64) {
            Ok(0) | Err(_) => return 0,
            Ok(n) => done += n,
        }
    }
    1
}

/// Lets PDFium read from a byte slice (JPEG data for image objects).
#[repr(C)] // `access` must be first
struct MemSource<'a> {
    access: FPDF_FILEACCESS,
    bytes: &'a [u8],
}

unsafe extern "C" fn read_mem(param: *mut c_void, position: c_ulong, buf: *mut u8, size: c_ulong) -> c_int {
    let source = &*(param as *const MemSource);
    let (start, len) = (position as usize, size as usize);
    let Some(chunk) = source.bytes.get(start..start + len) else { return 0 };
    std::ptr::copy_nonoverlapping(chunk.as_ptr(), buf, len);
    1
}

#[repr(C)] // `base` must be first: PDFium hands its address back to `write_block`
struct Sink<'a> {
    base: FPDF_FILEWRITE,
    out: &'a mut dyn Write,
    failed: bool,
}

unsafe extern "C" fn write_block(this: *mut FPDF_FILEWRITE, data: *const c_void, size: c_ulong) -> c_int {
    let sink = &mut *(this as *mut Sink);
    let bytes = std::slice::from_raw_parts(data as *const u8, size as usize);
    if sink.out.write_all(bytes).is_err() {
        sink.failed = true;
        return 0;
    }
    1
}

pub struct Document {
    handle: FPDF_DOCUMENT,
    /// Page sizes in PDF points (1/72 inch), read without loading pages.
    pub page_sizes: Vec<(f32, f32)>,
    /// None for a document that has never been saved.
    pub path: Option<PathBuf>,
    /// Overlays read out of the file while rendering, waiting for the tab to
    /// take them. They are removed from the PDF itself as they are read.
    pub found: Vec<Overlay>,
    /// The password it was opened with; copies and reopening need it.
    password: Option<String>,
    /// Stamps written by our own last save carry this tag. The tab still has
    /// them, so when they're read back they are only removed, not returned.
    skip_sid: Option<String>,
    /// The page whose text was last looked up (text-snapping highlights).
    text: RefCell<Option<TextPage>>,
    _backing: Backing,
}

/// A loaded page and its text layer, kept between lookups.
struct TextPage {
    index: usize,
    page: FPDF_PAGE,
    text: FPDF_TEXTPAGE,
}

impl Drop for TextPage {
    fn drop(&mut self) {
        unsafe {
            (api().FPDFText_ClosePage)(self.text);
            (api().FPDF_ClosePage)(self.page);
        }
    }
}

/// Why a file didn't open.
pub enum OpenError {
    /// It's encrypted: no password was given, or the one given was wrong.
    Password,
    Failed(String),
}

impl OpenError {
    pub fn message(self, path: &Path) -> String {
        match self {
            OpenError::Password => format!("{} needs a password.", path.display()),
            OpenError::Failed(e) => e,
        }
    }
}

/// How `export` writes the copy.
pub struct Export<'a> {
    /// Burn overlays into the page content instead of writing editable stamps.
    pub flatten: bool,
    /// Leave out any encryption (copies for printing, images, …).
    pub plain: bool,
    /// Tag written stamps with this id (see `Document::skip_sid`).
    pub sid: Option<&'a str>,
}

impl Drop for Document {
    fn drop(&mut self) {
        // Pages close before the document, and both before the backing.
        self.text.borrow_mut().take();
        unsafe { (api().FPDF_CloseDocument)(self.handle) };
    }
}

impl Document {
    pub fn open(path: &Path, password: Option<&str>) -> Result<Self, OpenError> {
        let fail = |why: &str| OpenError::Failed(format!("Could not open {}: {why}", path.display()));
        let file = File::open(path).map_err(|e| fail(&e.to_string()))?;
        let len = file.metadata().map_err(|e| fail(&e.to_string()))?.len();
        let len = c_ulong::try_from(len).map_err(|_| fail("files over 4 GB are not supported yet"))?;
        let pw = password.map(|p| CString::new(p).unwrap_or_default());
        let mut source = Box::new(FileSource {
            access: FPDF_FILEACCESS { m_FileLen: len, m_GetBlock: Some(read_block), m_Param: std::ptr::null_mut() },
            file,
        });
        source.access.m_Param = &mut *source as *mut FileSource as *mut c_void;
        let pw_ptr = pw.as_ref().map_or(std::ptr::null(), |p| p.as_ptr());
        let handle = unsafe { (api().FPDF_LoadCustomDocument)(&mut source.access, pw_ptr) };
        if handle.is_null() {
            if unsafe { (api().FPDF_GetLastError)() } == pdfium::FPDF_ERR_PASSWORD {
                return Err(OpenError::Password);
            }
            return Err(fail(pdfium::last_error()));
        }
        let mut doc = Self::wrap(handle, Some(path.to_path_buf()), Backing::File(source));
        doc.password = password.map(String::from);
        Ok(doc)
    }

    /// A document parsed from an in-memory copy.
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self, String> {
        Self::from_bytes_with(bytes, None)
    }

    fn from_bytes_with(bytes: Vec<u8>, password: Option<&str>) -> Result<Self, String> {
        let pw = password.map(|p| CString::new(p).unwrap_or_default());
        let pw_ptr = pw.as_ref().map_or(std::ptr::null(), |p| p.as_ptr());
        let handle = unsafe { (api().FPDF_LoadMemDocument64)(bytes.as_ptr().cast(), bytes.len(), pw_ptr) };
        if handle.is_null() {
            return Err(format!("Could not read document: {}", pdfium::last_error()));
        }
        let mut doc = Self::wrap(handle, None, Backing::Memory(bytes));
        doc.password = password.map(String::from);
        Ok(doc)
    }

    /// An in-memory copy (same password).
    fn copy(&self) -> Result<Self, String> {
        Self::from_bytes_with(self.to_bytes()?, self.password.as_deref())
    }

    /// A new, untitled document with one blank US Letter page.
    pub fn new_blank() -> Result<Self, String> {
        let mut doc = Self::new_empty()?;
        unsafe { (api().FPDF_ClosePage)((api().FPDFPage_New)(doc.handle, 0, 612.0, 792.0)) };
        doc.refresh();
        Ok(doc)
    }

    /// A new, untitled document holding copies of `indices` of `source`, in that order.
    pub fn from_pages(source: &Document, indices: &[usize]) -> Result<Self, String> {
        let mut doc = Self::new_empty()?;
        doc.import_pages(source, Some(indices), 0)?;
        Ok(doc)
    }

    pub fn new_empty() -> Result<Self, String> {
        let handle = unsafe { (api().FPDF_CreateNewDocument)() };
        if handle.is_null() {
            return Err("Could not create document".into());
        }
        Ok(Self::wrap(handle, None, Backing::None))
    }

    fn wrap(handle: FPDF_DOCUMENT, path: Option<PathBuf>, backing: Backing) -> Self {
        let mut doc = Self {
            handle,
            page_sizes: Vec::new(),
            path,
            found: Vec::new(),
            password: None,
            skip_sid: None,
            text: RefCell::new(None),
            _backing: backing,
        };
        doc.refresh();
        doc
    }

    /// Re-reads page sizes after the page list changes.
    fn refresh(&mut self) {
        self.text.borrow_mut().take(); // page indices may have changed
        let count = unsafe { (api().FPDF_GetPageCount)(self.handle) }.max(0);
        self.page_sizes = (0..count)
            .map(|i| {
                let mut size = FS_SIZEF { width: 612.0, height: 792.0 };
                unsafe { (api().FPDF_GetPageSizeByIndexF)(self.handle, i, &mut size) };
                (size.width.max(1.0), size.height.max(1.0))
            })
            .collect();
    }

    pub fn page_count(&self) -> usize {
        self.page_sizes.len()
    }

    /// File name, or None for a never-saved document.
    pub fn file_name(&self) -> Option<String> {
        self.path.as_ref()?.file_name().map(|n| n.to_string_lossy().into_owned())
    }

    // ---- page edits --------------------------------------------------------

    /// Moves page `from` so that it ends up at index `to` (indices after the move).
    pub fn move_page(&mut self, from: usize, to: usize) -> Result<(), String> {
        let index = from as c_int;
        if unsafe { (api().FPDF_MovePages)(self.handle, &index, 1, to as c_int) } == 0 {
            return Err(format!("Could not move page {}", from + 1));
        }
        self.refresh();
        Ok(())
    }

    /// Inserts copies of `source`'s pages `indices` (None: all of them) at
    /// position `at` in this document.
    pub fn import_pages(&mut self, source: &Document, indices: Option<&[usize]>, at: usize) -> Result<(), String> {
        let list: Vec<c_int> = indices.map(|i| i.iter().map(|&p| p as c_int).collect()).unwrap_or_default();
        let (ptr, len) = if indices.is_some() { (list.as_ptr(), list.len()) } else { (std::ptr::null(), 0) };
        if unsafe { (api().FPDF_ImportPagesByIndex)(self.handle, source.handle, ptr, len as c_ulong, at as c_int) } == 0 {
            return Err("Could not copy the pages".into());
        }
        self.refresh();
        Ok(())
    }

    /// Deletes `count` pages starting at `at`.
    pub fn delete_pages(&mut self, at: usize, count: usize) {
        for _ in 0..count {
            unsafe { (api().FPDFPage_Delete)(self.handle, at as c_int) };
        }
        self.refresh();
    }

    /// Looks up page `index`'s text layer (cached for the next call).
    fn with_text<R>(&self, index: usize, f: impl FnOnce(&TextPage) -> R) -> Option<R> {
        let mut cache = self.text.borrow_mut();
        if cache.as_ref().is_none_or(|t| t.index != index) {
            *cache = None;
            unsafe {
                let page = (api().FPDF_LoadPage)(self.handle, index as c_int);
                if page.is_null() {
                    return None;
                }
                let text = (api().FPDFText_LoadPage)(page);
                if text.is_null() {
                    (api().FPDF_ClosePage)(page);
                    return None;
                }
                *cache = Some(TextPage { index, page, text });
            }
        }
        cache.as_ref().map(f)
    }

    /// The text between two points on page `index` (display points), as one
    /// box per line, in display points. None if `from` isn't on any text.
    pub fn text_boxes_between(&self, index: usize, from: (f32, f32), to: (f32, f32)) -> Option<Vec<[f32; 4]>> {
        let size = *self.page_sizes.get(index)?;
        self.with_text(index, |t| unsafe {
            let a = api();
            let map = stamp::PageMap::new(t.page, size);
            let char_at = |(x, y): (f32, f32), tolerance: f64| {
                let (px, py) = map.to_page(x, y);
                (a.FPDFText_GetCharIndexAtPos)(t.text, px, py, tolerance, tolerance)
            };
            let start = char_at(from, 2.0);
            if start < 0 {
                return None;
            }
            // The end may be a little off the text; widen the search, then
            // fall back to the start character alone.
            let end = [2.0, 8.0, 20.0].iter().map(|&tol| char_at(to, tol)).find(|&i| i >= 0).unwrap_or(start);
            let (first, last) = (start.min(end), start.max(end));
            let boxes = (0..(a.FPDFText_CountRects)(t.text, first, last - first + 1))
                .filter_map(|r| {
                    let (mut l, mut tp, mut rt, mut b) = (0.0, 0.0, 0.0, 0.0);
                    if (a.FPDFText_GetRect)(t.text, r, &mut l, &mut tp, &mut rt, &mut b) == 0 {
                        return None;
                    }
                    let (x0, y0) = map.to_display(l, tp);
                    let (x1, y1) = map.to_display(rt, b);
                    Some([x0.min(x1), y0.min(y1), (x1 - x0).abs(), (y1 - y0).abs()])
                })
                .collect();
            Some(boxes)
        })
        .flatten()
    }

    /// Re-encodes large pictures as JPEG at no more than `max_dpi` where that
    /// makes them smaller. Pictures with transparency are left alone.
    /// Returns how many were replaced.
    pub fn shrink_images(&mut self, max_dpi: f32, quality: f32) -> usize {
        let a = api();
        let mut replaced = 0;
        for i in 0..self.page_count() {
            unsafe {
                let mut page = (a.FPDF_LoadPage)(self.handle, i as c_int);
                if page.is_null() {
                    continue;
                }
                let mut changed = false;
                for k in 0..(a.FPDFPage_CountObjects)(page) {
                    let obj = (a.FPDFPage_GetObject)(page, k);
                    if obj.is_null() || (a.FPDFPageObj_GetType)(obj) != pdfium::FPDF_PAGEOBJ_IMAGE {
                        continue;
                    }
                    if shrink_image(self.handle, &mut page, obj, max_dpi, quality) {
                        replaced += 1;
                        changed = true;
                    }
                }
                if changed {
                    (a.FPDFPage_GenerateContent)(page);
                }
                (a.FPDF_ClosePage)(page);
            }
        }
        replaced
    }

    /// Turns page `index` by `quarters` quarter turns clockwise (negative:
    /// counterclockwise).
    pub fn rotate_page(&mut self, index: usize, quarters: i32) -> Result<(), String> {
        let a = api();
        unsafe {
            let page = (a.FPDF_LoadPage)(self.handle, index as c_int);
            if page.is_null() {
                return Err(format!("Could not rotate page {}", index + 1));
            }
            let now = (a.FPDFPage_GetRotation)(page);
            (a.FPDFPage_SetRotation)(page, (now + quarters).rem_euclid(4));
            (a.FPDF_ClosePage)(page);
        }
        self.refresh();
        Ok(())
    }

    /// Every occurrence of `query` in the page text: per match, its page and
    /// the boxes (x, y, w, h in display points) it covers, in reading order.
    pub fn find(&self, query: &str, match_case: bool) -> Vec<(usize, Vec<[f32; 4]>)> {
        let a = api();
        let mut found = Vec::new();
        if query.is_empty() {
            return found;
        }
        let what: Vec<u16> = query.encode_utf16().chain([0]).collect();
        let flags = if match_case { pdfium::FPDF_MATCHCASE } else { 0 };
        for (i, &size) in self.page_sizes.iter().enumerate() {
            unsafe {
                let page = (a.FPDF_LoadPage)(self.handle, i as c_int);
                if page.is_null() {
                    continue;
                }
                let text = (a.FPDFText_LoadPage)(page);
                if !text.is_null() {
                    let map = stamp::PageMap::new(page, size);
                    let search = (a.FPDFText_FindStart)(text, what.as_ptr(), flags, 0);
                    while !search.is_null() && (a.FPDFText_FindNext)(search) != 0 {
                        let (start, count) = ((a.FPDFText_GetSchResultIndex)(search), (a.FPDFText_GetSchCount)(search));
                        let boxes = (0..(a.FPDFText_CountRects)(text, start, count))
                            .filter_map(|r| {
                                let (mut l, mut t, mut rt, mut b) = (0.0, 0.0, 0.0, 0.0);
                                if (a.FPDFText_GetRect)(text, r, &mut l, &mut t, &mut rt, &mut b) == 0 {
                                    return None;
                                }
                                let (x0, y0) = map.to_display(l, t);
                                let (x1, y1) = map.to_display(rt, b);
                                Some([x0.min(x1), y0.min(y1), (x1 - x0).abs(), (y1 - y0).abs()])
                            })
                            .collect();
                        found.push((i, boxes));
                    }
                    if !search.is_null() {
                        (a.FPDFText_FindClose)(search);
                    }
                    (a.FPDFText_ClosePage)(text);
                }
                (a.FPDF_ClosePage)(page);
            }
        }
        found
    }

    // ---- saving ------------------------------------------------------------

    fn write_to(&self, out: &mut dyn Write, plain: bool) -> Result<(), String> {
        let mut sink = Sink { base: FPDF_FILEWRITE { version: 1, WriteBlock: Some(write_block) }, out, failed: false };
        let flags = pdfium::FPDF_NO_INCREMENTAL | if plain { pdfium::FPDF_REMOVE_SECURITY } else { 0 };
        let ok = unsafe { (api().FPDF_SaveAsCopy)(self.handle, &mut sink.base, flags) } != 0;
        if !ok || sink.failed {
            return Err("PDFium could not write the document".into());
        }
        Ok(())
    }

    /// The document as a PDF file in memory, encryption kept.
    pub fn to_bytes(&self) -> Result<Vec<u8>, String> {
        let mut bytes = Vec::new();
        self.write_to(&mut bytes, false)?;
        Ok(bytes)
    }

    /// A working copy with `overlays` written in: as editable stamps, or
    /// flattened into the pages. This document itself is left unchanged.
    pub fn export_doc(&self, overlays: &[Overlay], how: &Export) -> Result<Document, String> {
        let flatten = how.flatten;
        let copy = self.copy()?;
        let a = api();
        let mut fonts = stamp::Fonts::new(copy.handle);
        for (i, &size) in copy.page_sizes.iter().enumerate() {
            let mine: Vec<&Overlay> = overlays.iter().filter(|o| o.page == i).collect();
            // Flattening also covers stamps on pages never shown, still in the file.
            if mine.is_empty() && !flatten {
                continue;
            }
            unsafe {
                let page = (a.FPDF_LoadPage)(copy.handle, i as c_int);
                if page.is_null() {
                    continue;
                }
                let unread = if flatten { stamp::take(copy.handle, page, i, size, None) } else { Vec::new() };
                let all: Vec<&Overlay> = unread.iter().chain(mine).collect();
                stamp::write(copy.handle, page, size, &all, flatten, how.sid, &mut fonts);
                (a.FPDF_ClosePage)(page);
            }
        }
        drop(fonts);
        Ok(copy)
    }

    /// `export_doc`, serialized.
    pub fn export(&self, overlays: &[Overlay], how: &Export) -> Result<Vec<u8>, String> {
        let flatten = how.flatten;
        let doc = if overlays.is_empty() && !flatten { None } else { Some(self.export_doc(overlays, how)?) };
        let mut bytes = Vec::new();
        doc.as_ref().unwrap_or(self).write_to(&mut bytes, how.plain)?;
        Ok(bytes)
    }

    /// Writes PDF bytes to `path` through a temp file renamed into place.
    pub fn write_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
        let file_name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let temp = path.with_file_name(format!(".{file_name}.bestpdf-tmp"));
        let result = std::fs::write(&temp, bytes)
            .map_err(|e| e.to_string())
            .and_then(|()| std::fs::rename(&temp, path).map_err(|e| e.to_string()));
        if let Err(e) = result {
            let _ = std::fs::remove_file(&temp);
            return Err(format!("Could not save {}: {e}", path.display()));
        }
        Ok(())
    }

    /// Writes the document with `overlays` to `path`, then reloads `self`
    /// from there (with the same password). Overlays saved as stamps are
    /// tagged, so when pages render again they're cleared out of the reloaded
    /// file without being handed back: the tab still has them.
    pub fn save_as(&mut self, path: &Path, overlays: &[Overlay], flatten: bool) -> Result<(), String> {
        let sid = format!("{:x}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos()));
        let bytes = self.export(overlays, &Export { flatten, plain: false, sid: Some(&sid) })?;
        Self::write_file(path, &bytes)?;
        let password = self.password.clone();
        *self = Self::open(path, password.as_deref())
            .map_err(|e| format!("Saved, but could not reopen the file. {}", e.message(path)))?;
        if !flatten {
            self.skip_sid = Some(sid);
        }
        Ok(())
    }

    // ---- rendering ---------------------------------------------------------

    /// Height in pixels of page `index` scaled to `width`.
    pub fn page_height(&self, index: usize, width: i32) -> i32 {
        let (pw, ph) = self.page_sizes.get(index).copied().unwrap_or((612.0, 792.0));
        ((width as f32 * ph / pw).round() as i32).max(1)
    }

    /// Renders one page scaled to `width` pixels (height follows aspect ratio):
    /// all of it, or just `region` (x0, y0, x1, y1 in those pixels).
    /// Best PDF stamps on the page are moved into `found` first.
    pub fn render(&mut self, index: usize, width: i32, region: Option<(i32, i32, i32, i32)>) -> Result<PageBitmap, String> {
        let (pw, ph) = *self.page_sizes.get(index).ok_or_else(|| format!("No page {}", index + 1))?;
        let (page_w, page_h) = (width.max(1), self.page_height(index, width));
        let (x0, y0, x1, y1) = region.unwrap_or((0, 0, page_w, page_h));
        let (x0, y0) = (x0.clamp(0, page_w - 1), y0.clamp(0, page_h - 1));
        let (w, h) = ((x1.min(page_w) - x0).max(1), (y1.min(page_h) - y0).max(1));
        let mut pixels = vec![0u8; w as usize * h as usize * 4];
        let a = api();
        unsafe {
            let bitmap = (a.FPDFBitmap_CreateEx)(w, h, pdfium::FPDFBITMAP_BGRX, pixels.as_mut_ptr().cast(), w * 4);
            if bitmap.is_null() {
                return Err(format!("Page {}: out of memory", index + 1));
            }
            (a.FPDFBitmap_FillRect)(bitmap, 0, 0, w, h, 0xFFFF_FFFF);
            let page = (a.FPDF_LoadPage)(self.handle, index as c_int);
            if !page.is_null() {
                self.found.extend(stamp::take(self.handle, page, index, (pw, ph), self.skip_sid.as_deref()));
                (a.FPDF_RenderPageBitmap)(bitmap, page, -x0, -y0, page_w, page_h, 0, pdfium::FPDF_ANNOT);
                (a.FPDF_ClosePage)(page);
            }
            (a.FPDFBitmap_Destroy)(bitmap); // our buffer is not freed
        }
        Ok(PageBitmap { width: w, height: h, pixels, page_w, x0, y0 })
    }

    /// Draws a page as vector output into a device context (printing).
    /// `rotate` is in quarter turns clockwise.
    pub fn render_to_dc(&self, dc: HDC, index: usize, x: i32, y: i32, w: i32, h: i32, rotate: i32) {
        let a = api();
        unsafe {
            let page = (a.FPDF_LoadPage)(self.handle, index as c_int);
            if !page.is_null() {
                (a.FPDF_RenderPage)(dc, page, x, y, w, h, rotate, pdfium::FPDF_ANNOT | pdfium::FPDF_PRINTING);
                (a.FPDF_ClosePage)(page);
            }
        }
    }
}

/// Replaces one picture's data with a smaller JPEG when it's denser than
/// `max_dpi` and that makes it smaller. Pictures with transparency are kept.
unsafe fn shrink_image(doc: FPDF_DOCUMENT, page: &mut FPDF_PAGE, obj: pdfium::FPDF_PAGEOBJECT, max_dpi: f32, quality: f32) -> bool {
    let a = api();
    let (mut pw, mut ph) = (0, 0);
    if (a.FPDFImageObj_GetImagePixelSize)(obj, &mut pw, &mut ph) == 0 || pw < 64 || ph < 64 {
        return false;
    }
    let (mut l, mut b, mut r, mut t) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
    if (a.FPDFPageObj_GetBounds)(obj, &mut l, &mut b, &mut r, &mut t) == 0 {
        return false;
    }
    let (inches_w, inches_h) = ((r - l) / 72.0, (t - b) / 72.0);
    if inches_w <= 0.0 || inches_h <= 0.0 {
        return false;
    }
    let dpi = (pw as f32 / inches_w).max(ph as f32 / inches_h);
    if dpi <= max_dpi * 1.1 {
        return false;
    }
    let k = max_dpi / dpi;
    let (tw, th) = (((pw as f32 * k).round() as i32).max(1), ((ph as f32 * k).round() as i32).max(1));
    let raw_len = (a.FPDFImageObj_GetImageDataRaw)(obj, std::ptr::null_mut(), 0) as usize;

    // Full-resolution pixels with any mask applied: render at one pixel per pixel.
    let mut matrix = FS_MATRIX::default();
    (a.FPDFPageObj_GetMatrix)(obj, &mut matrix);
    (a.FPDFPageObj_SetMatrix)(obj, &FS_MATRIX { a: pw as f32, d: ph as f32, ..Default::default() });
    let bmp = (a.FPDFImageObj_GetRenderedBitmap)(doc, *page, obj);
    (a.FPDFPageObj_SetMatrix)(obj, &matrix);
    if bmp.is_null() {
        return false;
    }
    let (w, h, bgra) = stamp::bitmap_bgra(bmp);
    (a.FPDFBitmap_Destroy)(bmp);
    if bgra.chunks_exact(4).any(|p| p[3] < 250) {
        return false;
    }
    let small = Image::new(w, h, bgra).resized(tw, th);
    let Ok(jpeg) = imageio::jpeg_bytes(small.w, small.h, &small.bgra, quality) else { return false };
    if raw_len > 0 && jpeg.len() >= raw_len {
        return false;
    }
    let mut source = MemSource {
        access: FPDF_FILEACCESS { m_FileLen: jpeg.len() as c_ulong, m_GetBlock: Some(read_mem), m_Param: std::ptr::null_mut() },
        bytes: &jpeg,
    };
    source.access.m_Param = &mut source as *mut MemSource as *mut c_void;
    let ok = (a.FPDFImageObj_LoadJpegFileInline)(page, 1, obj, &mut source.access) != 0;
    (a.FPDFPageObj_SetMatrix)(obj, &matrix);
    ok
}

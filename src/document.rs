//! Safe wrapper over PDFium. Everything else in the app talks to this module,
//! never to pdfium.rs directly, so the engine stays swappable. (stamp.rs, which
//! turns overlays into PDF objects and back, is the one helper it uses.)

use std::ffi::{c_int, c_ulong, c_void};
use std::fs::File;
use std::io::Write;
use std::os::windows::fs::FileExt;
use std::path::{Path, PathBuf};

use windows::Win32::Graphics::Gdi::HDC;

use crate::overlay::Overlay;
use crate::pdfium::{self, api, FPDF_DOCUMENT, FPDF_FILEACCESS, FPDF_FILEWRITE, FS_SIZEF};
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
    _backing: Backing,
}

impl Drop for Document {
    fn drop(&mut self) {
        // Runs before the fields drop, so the backing outlives the handle.
        unsafe { (api().FPDF_CloseDocument)(self.handle) };
    }
}

impl Document {
    pub fn open(path: &Path) -> Result<Self, String> {
        let fail = |why: &str| format!("Could not open {}: {why}", path.display());
        let file = File::open(path).map_err(|e| fail(&e.to_string()))?;
        let len = file.metadata().map_err(|e| fail(&e.to_string()))?.len();
        let len = c_ulong::try_from(len).map_err(|_| fail("files over 4 GB are not supported yet"))?;
        let mut source = Box::new(FileSource {
            access: FPDF_FILEACCESS { m_FileLen: len, m_GetBlock: Some(read_block), m_Param: std::ptr::null_mut() },
            file,
        });
        source.access.m_Param = &mut *source as *mut FileSource as *mut c_void;
        let handle = unsafe { (api().FPDF_LoadCustomDocument)(&mut source.access, std::ptr::null()) };
        if handle.is_null() {
            return Err(fail(pdfium::last_error()));
        }
        Ok(Self::wrap(handle, Some(path.to_path_buf()), Backing::File(source)))
    }

    /// A document parsed from an in-memory copy (used for printing).
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self, String> {
        let handle = unsafe { (api().FPDF_LoadMemDocument64)(bytes.as_ptr().cast(), bytes.len(), std::ptr::null()) };
        if handle.is_null() {
            return Err(format!("Could not read document: {}", pdfium::last_error()));
        }
        Ok(Self::wrap(handle, None, Backing::Memory(bytes)))
    }

    /// A new, untitled document with one blank US Letter page.
    pub fn new_blank() -> Result<Self, String> {
        let mut doc = Self::new_empty()?;
        unsafe { (api().FPDF_ClosePage)((api().FPDFPage_New)(doc.handle, 0, 612.0, 792.0)) };
        doc.refresh();
        Ok(doc)
    }

    /// A new, untitled document holding a copy of one page of `source`.
    pub fn from_page(source: &Document, index: usize) -> Result<Self, String> {
        let mut doc = Self::new_empty()?;
        doc.import_page(source, index, 0)?;
        Ok(doc)
    }

    fn new_empty() -> Result<Self, String> {
        let handle = unsafe { (api().FPDF_CreateNewDocument)() };
        if handle.is_null() {
            return Err("Could not create document".into());
        }
        Ok(Self::wrap(handle, None, Backing::None))
    }

    fn wrap(handle: FPDF_DOCUMENT, path: Option<PathBuf>, backing: Backing) -> Self {
        let mut doc = Self { handle, page_sizes: Vec::new(), path, found: Vec::new(), _backing: backing };
        doc.refresh();
        doc
    }

    /// Re-reads page sizes after the page list changes.
    fn refresh(&mut self) {
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

    /// Inserts a copy of `source`'s page `index` at position `at` in this document.
    pub fn import_page(&mut self, source: &Document, index: usize, at: usize) -> Result<(), String> {
        let src = index as c_int;
        if unsafe { (api().FPDF_ImportPagesByIndex)(self.handle, source.handle, &src, 1, at as c_int) } == 0 {
            return Err(format!("Could not copy page {}", index + 1));
        }
        self.refresh();
        Ok(())
    }

    /// Inserts a copy of this document's own page `index` at position `at`.
    /// Goes through a scratch document: PDFium imports between two documents.
    pub fn duplicate_page(&mut self, index: usize, at: usize) -> Result<(), String> {
        let scratch = Self::from_page(self, index)?;
        self.import_page(&scratch, 0, at)
    }

    pub fn delete_page(&mut self, index: usize) {
        unsafe { (api().FPDFPage_Delete)(self.handle, index as c_int) };
        self.refresh();
    }

    // ---- saving ------------------------------------------------------------

    fn write_to(&self, out: &mut dyn Write) -> Result<(), String> {
        let mut sink = Sink { base: FPDF_FILEWRITE { version: 1, WriteBlock: Some(write_block) }, out, failed: false };
        let ok = unsafe { (api().FPDF_SaveAsCopy)(self.handle, &mut sink.base, pdfium::FPDF_NO_INCREMENTAL) } != 0;
        if !ok || sink.failed {
            return Err("PDFium could not write the document".into());
        }
        Ok(())
    }

    fn to_bytes(&self) -> Result<Vec<u8>, String> {
        let mut bytes = Vec::new();
        self.write_to(&mut bytes)?;
        Ok(bytes)
    }

    /// Serialized copy with `overlays` written in: as editable stamps, or
    /// flattened into the pages. This document itself is left unchanged.
    pub fn export(&self, overlays: &[Overlay], flatten: bool) -> Result<Vec<u8>, String> {
        if overlays.is_empty() && !flatten {
            return self.to_bytes();
        }
        let copy = Self::from_bytes(self.to_bytes()?)?;
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
                let unread = if flatten { stamp::take(copy.handle, page, i, size) } else { Vec::new() };
                let all: Vec<&Overlay> = unread.iter().chain(mine).collect();
                stamp::write(copy.handle, page, size, &all, flatten, &mut fonts);
                (a.FPDF_ClosePage)(page);
            }
        }
        drop(fonts);
        copy.to_bytes()
    }

    /// Writes the document with `overlays` to `path`, then reloads `self` from
    /// there. Overlays saved as stamps come back through `found` as pages render.
    ///
    /// PDFium reads the source file lazily, so writing over it directly would
    /// corrupt the output. We write a sibling temp file and rename it into place;
    /// the old handle keeps reading the old (now unlinked) data until replaced.
    pub fn save_as(&mut self, path: &Path, overlays: &[Overlay], flatten: bool) -> Result<(), String> {
        let file_name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let temp = path.with_file_name(format!(".{file_name}.bestpdf-tmp"));
        let result = self
            .export(overlays, flatten)
            .and_then(|bytes| std::fs::write(&temp, bytes).map_err(|e| e.to_string()))
            .and_then(|()| std::fs::rename(&temp, path).map_err(|e| e.to_string()));
        if let Err(e) = result {
            let _ = std::fs::remove_file(&temp);
            return Err(format!("Could not save {}: {e}", path.display()));
        }
        *self = Self::open(path).map_err(|e| format!("Saved, but could not reopen the file. {e}"))?;
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
                self.found.extend(stamp::take(self.handle, page, index, (pw, ph)));
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

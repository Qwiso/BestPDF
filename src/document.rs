//! Thin wrapper over PDFium. Everything else in the app talks to this module
//! (and print.rs, for the one call pdfium-render doesn't expose), never to
//! pdfium-render directly, so the engine stays swappable.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use pdfium_render::prelude::*;

static PDFIUM: OnceLock<Pdfium> = OnceLock::new();

/// Binds pdfium.dll from next to the executable (portable layout).
/// Called once at startup; the library lives for the whole process.
pub fn init() -> Result<(), String> {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));
    let bindings = Pdfium::bind_to_library(Pdfium::pdfium_platform_library_name_at_path(&exe_dir))
        .map_err(|e| format!("Could not load pdfium.dll from {}: {e}", exe_dir.display()))?;
    let _ = PDFIUM.set(Pdfium::new(bindings));
    Ok(())
}

fn pdfium() -> &'static Pdfium {
    PDFIUM.get().expect("document::init() must run first")
}

/// A rendered page in 32-bit BGRx, top-down, stride = width * 4.
pub struct PageBitmap {
    pub width: i32,
    pub height: i32,
    pub pixels: Vec<u8>,
}

pub struct Document {
    doc: PdfDocument<'static>,
    /// Page sizes in PDF points (1/72 inch), read once without loading pages.
    pub page_sizes: Vec<(f32, f32)>,
    /// None for a new document that has never been saved.
    pub path: Option<PathBuf>,
}

impl Document {
    pub fn open(path: &Path) -> Result<Self, String> {
        let doc = pdfium()
            .load_pdf_from_file(path, None)
            .map_err(|e| format!("Could not open {}: {e}", path.display()))?;
        Self::wrap(doc, Some(path.to_path_buf()))
    }

    /// A new, untitled document with one blank US Letter page.
    pub fn new_blank() -> Result<Self, String> {
        let mut doc = pdfium().create_new_pdf().map_err(|e| format!("Could not create document: {e}"))?;
        doc.pages_mut()
            // Exact 8.5 × 11 in; the crate's USLetter preset is rounded to whole mm.
            .create_page_at_end(PdfPagePaperSize::Custom(PdfPoints::new(612.0), PdfPoints::new(792.0)))
            .map_err(|e| format!("Could not add page: {e}"))?;
        Self::wrap(doc, None)
    }

    fn wrap(doc: PdfDocument<'static>, path: Option<PathBuf>) -> Result<Self, String> {
        let page_sizes = doc
            .pages()
            .page_sizes()
            .map_err(|e| format!("Could not read page sizes: {e}"))?
            .iter()
            .map(|r| (r.width().value, r.height().value))
            .collect();
        Ok(Self { doc, page_sizes, path })
    }

    pub fn page_count(&self) -> usize {
        self.page_sizes.len()
    }

    /// File name, or None for a never-saved document.
    pub fn file_name(&self) -> Option<String> {
        self.path.as_ref()?.file_name().map(|n| n.to_string_lossy().into_owned())
    }

    /// Writes the document to `path`, then reloads `self` from there.
    ///
    /// PDFium reads the source file lazily, so writing over it directly would
    /// corrupt the output. We write a sibling temp file and rename it into place;
    /// the old handle keeps reading the old (now unlinked) data until replaced.
    pub fn save_as(&mut self, path: &Path) -> Result<(), String> {
        let file_name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let temp = path.with_file_name(format!(".{file_name}.bestpdf-tmp"));
        let result = self
            .doc
            .save_to_file(&temp)
            .map_err(|e| e.to_string())
            .and_then(|()| std::fs::rename(&temp, path).map_err(|e| e.to_string()));
        if let Err(e) = result {
            let _ = std::fs::remove_file(&temp);
            return Err(format!("Could not save {}: {e}", path.display()));
        }
        *self = Self::open(path).map_err(|e| format!("Saved, but could not reopen the file. {e}"))?;
        Ok(())
    }

    /// Serialized copy of the current document (used for printing).
    pub fn to_bytes(&self) -> Result<Vec<u8>, String> {
        self.doc.save_to_bytes().map_err(|e| format!("Could not prepare document: {e}"))
    }

    /// Renders one page scaled to `width` pixels (height follows aspect ratio).
    pub fn render(&self, index: usize, width: i32) -> Result<PageBitmap, String> {
        let page = self
            .doc
            .pages()
            .get(index as PdfPageIndex)
            .map_err(|e| format!("Page {}: {e}", index + 1))?;
        let config = PdfRenderConfig::new()
            .set_target_width(width)
            .set_format(PdfBitmapFormat::BGRx)
            .set_reverse_byte_order(false) // GDI wants native BGR; the crate defaults to RGB
            .render_annotations(true)
            .render_form_data(true);
        let bitmap = page
            .render_with_config(&config)
            .map_err(|e| format!("Page {}: {e}", index + 1))?;
        Ok(PageBitmap {
            width: bitmap.width(),
            height: bitmap.height(),
            pixels: bitmap.as_raw_bytes(),
        })
    }
}

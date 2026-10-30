//! Printing: the standard Windows print dialog, then vector output through
//! PDFium's FPDF_RenderPage straight into the printer DC (no rasterizing).
//!
//! pdfium-render doesn't expose FPDF_RenderPage or raw handles, so this module
//! resolves the handful of C functions it needs from the already-loaded
//! pdfium.dll and prints from a serialized copy of the current document.

use std::ffi::c_void;

use windows::core::{s, w, HSTRING, PCSTR, PCWSTR};
use windows::Win32::Foundation::{GlobalFree, HMODULE, HWND};
use windows::Win32::Graphics::Gdi::{DeleteDC, GetDeviceCaps, HDC, HORZRES, LOGPIXELSX, LOGPIXELSY, VERTRES};
use windows::Win32::Storage::Xps::{AbortDoc, EndDoc, EndPage, StartDocW, StartPage, DOCINFOW};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::UI::Controls::Dialogs::*;

type Doc = *mut c_void;
type Page = *mut c_void;

const FPDF_ANNOT: i32 = 0x01;
const FPDF_PRINTING: i32 = 0x800;

struct Api {
    load_mem: unsafe extern "C" fn(*const c_void, usize, PCSTR) -> Doc,
    close_doc: unsafe extern "C" fn(Doc),
    load_page: unsafe extern "C" fn(Doc, i32) -> Page,
    close_page: unsafe extern "C" fn(Page),
    page_w: unsafe extern "C" fn(Page) -> f32,
    page_h: unsafe extern "C" fn(Page) -> f32,
    render: unsafe extern "C" fn(HDC, Page, i32, i32, i32, i32, i32, i32),
}

impl Api {
    fn load() -> Option<Self> {
        unsafe {
            let dll: HMODULE = GetModuleHandleW(w!("pdfium.dll")).ok()?;
            macro_rules! sym {
                ($name:expr) => {
                    std::mem::transmute(GetProcAddress(dll, $name)?)
                };
            }
            Some(Self {
                load_mem: sym!(s!("FPDF_LoadMemDocument64")),
                close_doc: sym!(s!("FPDF_CloseDocument")),
                load_page: sym!(s!("FPDF_LoadPage")),
                close_page: sym!(s!("FPDF_ClosePage")),
                page_w: sym!(s!("FPDF_GetPageWidthF")),
                page_h: sym!(s!("FPDF_GetPageHeightF")),
                render: sym!(s!("FPDF_RenderPage")),
            })
        }
    }
}

/// What the user picked in the print dialog. Owns the printer DC.
struct PrintJob {
    dc: HDC,
    /// Zero-based, inclusive page ranges.
    ranges: Vec<(usize, usize)>,
}

impl Drop for PrintJob {
    fn drop(&mut self) {
        unsafe { let _ = DeleteDC(self.dc); }
    }
}

/// Shows the print dialog. Returns None if the user cancels.
fn ask(owner: HWND, page_count: usize) -> Option<PrintJob> {
    let mut ranges = [PRINTPAGERANGE { nFromPage: 1, nToPage: page_count as u32 }; 16];
    let mut dlg = PRINTDLGEXW {
        lStructSize: size_of::<PRINTDLGEXW>() as u32,
        hwndOwner: owner,
        Flags: PD_RETURNDC | PD_USEDEVMODECOPIESANDCOLLATE | PD_NOSELECTION | PD_NOCURRENTPAGE,
        nPageRanges: 1,
        nMaxPageRanges: ranges.len() as u32,
        lpPageRanges: ranges.as_mut_ptr(),
        nMinPage: 1,
        nMaxPage: page_count as u32,
        nCopies: 1,
        nStartPage: START_PAGE_GENERAL,
        ..Default::default()
    };
    let ok = unsafe { PrintDlgExW(&mut dlg) }.is_ok();
    unsafe {
        if !dlg.hDevMode.is_invalid() { let _ = GlobalFree(Some(dlg.hDevMode)); }
        if !dlg.hDevNames.is_invalid() { let _ = GlobalFree(Some(dlg.hDevNames)); }
    }
    if dlg.hDC.is_invalid() {
        return None;
    }
    if !ok || dlg.dwResultAction != PD_RESULT_PRINT {
        unsafe { let _ = DeleteDC(dlg.hDC); }
        return None;
    }
    let ranges = if dlg.Flags.contains(PD_PAGENUMS) {
        ranges[..dlg.nPageRanges as usize]
            .iter()
            .map(|r| {
                let (a, b) = (r.nFromPage.min(r.nToPage), r.nFromPage.max(r.nToPage));
                (a.max(1) as usize - 1, (b as usize).min(page_count) - 1)
            })
            .collect()
    } else {
        vec![(0, page_count - 1)]
    };
    Some(PrintJob { dc: dlg.hDC, ranges })
}

/// Prints `pdf_bytes`. Returns Ok(false) if the user cancelled.
pub fn print(owner: HWND, title: &str, pdf_bytes: &[u8], page_count: usize) -> Result<bool, String> {
    let api = Api::load().ok_or("This pdfium.dll does not support printing.")?;
    let Some(job) = ask(owner, page_count) else { return Ok(false) };

    unsafe {
        let doc = (api.load_mem)(pdf_bytes.as_ptr().cast(), pdf_bytes.len(), PCSTR::null());
        if doc.is_null() {
            return Err("Could not prepare the document for printing.".into());
        }
        let result = spool(&api, &job, doc, title);
        (api.close_doc)(doc);
        result.map(|()| true)
    }
}

unsafe fn spool(api: &Api, job: &PrintJob, doc: Doc, title: &str) -> Result<(), String> {
    let dc = job.dc;
    let name = HSTRING::from(title);
    let info = DOCINFOW { cbSize: size_of::<DOCINFOW>() as i32, lpszDocName: PCWSTR(name.as_ptr()), ..Default::default() };
    if StartDocW(dc, &info) <= 0 {
        return Err("The printer did not accept the job.".into());
    }

    // Printable area in device pixels, and its physical aspect (DPI can differ per axis).
    let (area_w, area_h) = (GetDeviceCaps(Some(dc), HORZRES), GetDeviceCaps(Some(dc), VERTRES));
    let (dpi_x, dpi_y) = (GetDeviceCaps(Some(dc), LOGPIXELSX) as f32, GetDeviceCaps(Some(dc), LOGPIXELSY) as f32);
    let paper_landscape = area_w as f32 / dpi_x > area_h as f32 / dpi_y;

    for &(first, last) in &job.ranges {
        for index in first..=last {
            let page = (api.load_page)(doc, index as i32);
            if page.is_null() {
                continue;
            }
            // Points → inches; rotate 90° when page and paper orientations disagree.
            let (mut pw, mut ph) = ((api.page_w)(page) / 72.0, (api.page_h)(page) / 72.0);
            let rotate = (pw > ph) != paper_landscape;
            if rotate {
                std::mem::swap(&mut pw, &mut ph);
            }
            // Fit inside the printable area, centered, aspect preserved.
            let scale = (area_w as f32 / dpi_x / pw).min(area_h as f32 / dpi_y / ph);
            let (w, h) = ((pw * scale * dpi_x) as i32, (ph * scale * dpi_y) as i32);
            let (x, y) = ((area_w - w) / 2, (area_h - h) / 2);

            let ok = StartPage(dc) > 0;
            if ok {
                (api.render)(dc, page, x, y, w, h, if rotate { 1 } else { 0 }, FPDF_ANNOT | FPDF_PRINTING);
            }
            (api.close_page)(page);
            if !ok || EndPage(dc) <= 0 {
                AbortDoc(dc);
                return Err(format!("Printing stopped at page {}.", index + 1));
            }
        }
    }
    EndDoc(dc);
    Ok(())
}

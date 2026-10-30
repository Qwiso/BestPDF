//! Printing: the standard Windows print dialog, then vector output through
//! PDFium straight into the printer DC (no rasterizing).
//!
//! Prints from a serialized copy of the document, so the open tab stays free
//! for repainting while the modal print dialog runs.

use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::{GlobalFree, HWND};
use windows::Win32::Graphics::Gdi::{DeleteDC, GetDeviceCaps, HDC, HORZRES, LOGPIXELSX, LOGPIXELSY, VERTRES};
use windows::Win32::Storage::Xps::{AbortDoc, EndDoc, EndPage, StartDocW, StartPage, DOCINFOW};
use windows::Win32::UI::Controls::Dialogs::*;

use crate::document::Document;

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
pub fn print(owner: HWND, title: &str, pdf_bytes: Vec<u8>, page_count: usize) -> Result<bool, String> {
    let Some(job) = ask(owner, page_count) else { return Ok(false) };
    let doc = Document::from_bytes(pdf_bytes)?;
    spool(&job, &doc, title).map(|()| true)
}

fn spool(job: &PrintJob, doc: &Document, title: &str) -> Result<(), String> {
    let dc = job.dc;
    let name = HSTRING::from(title);
    let info = DOCINFOW { cbSize: size_of::<DOCINFOW>() as i32, lpszDocName: PCWSTR(name.as_ptr()), ..Default::default() };
    unsafe {
        if StartDocW(dc, &info) <= 0 {
            return Err("The printer did not accept the job.".into());
        }

        // Printable area in device pixels, and its physical aspect (DPI can differ per axis).
        let (area_w, area_h) = (GetDeviceCaps(Some(dc), HORZRES), GetDeviceCaps(Some(dc), VERTRES));
        let (dpi_x, dpi_y) = (GetDeviceCaps(Some(dc), LOGPIXELSX) as f32, GetDeviceCaps(Some(dc), LOGPIXELSY) as f32);
        let paper_landscape = area_w as f32 / dpi_x > area_h as f32 / dpi_y;

        for &(first, last) in &job.ranges {
            for index in first..=last.min(doc.page_count().saturating_sub(1)) {
                // Points → inches; rotate 90° when page and paper orientations disagree.
                let (pw, ph) = doc.page_sizes[index];
                let (mut pw, mut ph) = (pw / 72.0, ph / 72.0);
                let rotate = (pw > ph) != paper_landscape;
                if rotate {
                    std::mem::swap(&mut pw, &mut ph);
                }
                // Fit inside the printable area, centered, aspect preserved.
                let scale = (area_w as f32 / dpi_x / pw).min(area_h as f32 / dpi_y / ph);
                let (w, h) = ((pw * scale * dpi_x) as i32, (ph * scale * dpi_y) as i32);
                let (x, y) = ((area_w - w) / 2, (area_h - h) / 2);

                if StartPage(dc) <= 0 {
                    AbortDoc(dc);
                    return Err(format!("Printing stopped at page {}.", index + 1));
                }
                doc.render_to_dc(dc, index, x, y, w, h, if rotate { 1 } else { 0 });
                if EndPage(dc) <= 0 {
                    AbortDoc(dc);
                    return Err(format!("Printing stopped at page {}.", index + 1));
                }
            }
        }
        EndDoc(dc);
    }
    Ok(())
}

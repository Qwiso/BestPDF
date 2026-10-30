//! Raw PDFium C API, resolved at runtime from pdfium.dll next to the exe.
//! Only the functions Best PDF uses; document.rs wraps them safely.
#![allow(non_snake_case, non_camel_case_types)]

use std::ffi::{c_char, c_int, c_ulong, c_void};
use std::path::Path;
use std::sync::OnceLock;

use windows::core::{s, HSTRING};
use windows::Win32::Graphics::Gdi::HDC;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

pub type FPDF_DOCUMENT = *mut c_void;
pub type FPDF_PAGE = *mut c_void;
pub type FPDF_BITMAP = *mut c_void;
pub type FPDF_BOOL = c_int;

pub const FPDF_ANNOT: c_int = 0x01;
pub const FPDF_PRINTING: c_int = 0x800;
pub const FPDFBITMAP_BGRX: c_int = 3; // Gray = 1, BGR = 2, BGRx = 3, BGRA = 4
/// FPDF_SaveAsCopy: write a complete new file (required after page edits).
pub const FPDF_NO_INCREMENTAL: u32 = 2;

#[repr(C)]
pub struct FS_SIZEF {
    pub width: f32,
    pub height: f32,
}

#[repr(C)]
pub struct FPDF_FILEACCESS {
    pub m_FileLen: c_ulong,
    pub m_GetBlock: Option<unsafe extern "C" fn(param: *mut c_void, position: c_ulong, buf: *mut u8, size: c_ulong) -> c_int>,
    pub m_Param: *mut c_void,
}

#[repr(C)]
pub struct FPDF_FILEWRITE {
    pub version: c_int,
    pub WriteBlock: Option<unsafe extern "C" fn(this: *mut FPDF_FILEWRITE, data: *const c_void, size: c_ulong) -> c_int>,
}

pub struct Api {
    pub FPDF_InitLibrary: unsafe extern "system" fn(),
    pub FPDF_GetLastError: unsafe extern "system" fn() -> c_ulong,
    pub FPDF_LoadCustomDocument: unsafe extern "system" fn(*mut FPDF_FILEACCESS, *const c_char) -> FPDF_DOCUMENT,
    pub FPDF_LoadMemDocument64: unsafe extern "system" fn(*const c_void, usize, *const c_char) -> FPDF_DOCUMENT,
    pub FPDF_CreateNewDocument: unsafe extern "system" fn() -> FPDF_DOCUMENT,
    pub FPDF_CloseDocument: unsafe extern "system" fn(FPDF_DOCUMENT),
    pub FPDF_GetPageCount: unsafe extern "system" fn(FPDF_DOCUMENT) -> c_int,
    pub FPDF_GetPageSizeByIndexF: unsafe extern "system" fn(FPDF_DOCUMENT, c_int, *mut FS_SIZEF) -> FPDF_BOOL,
    pub FPDF_LoadPage: unsafe extern "system" fn(FPDF_DOCUMENT, c_int) -> FPDF_PAGE,
    pub FPDF_ClosePage: unsafe extern "system" fn(FPDF_PAGE),
    pub FPDFPage_New: unsafe extern "system" fn(FPDF_DOCUMENT, c_int, f64, f64) -> FPDF_PAGE,
    pub FPDFPage_Delete: unsafe extern "system" fn(FPDF_DOCUMENT, c_int),
    pub FPDF_ImportPagesByIndex: unsafe extern "system" fn(FPDF_DOCUMENT, FPDF_DOCUMENT, *const c_int, c_ulong, c_int) -> FPDF_BOOL,
    pub FPDF_MovePages: unsafe extern "system" fn(FPDF_DOCUMENT, *const c_int, c_ulong, c_int) -> FPDF_BOOL,
    pub FPDF_SaveAsCopy: unsafe extern "system" fn(FPDF_DOCUMENT, *mut FPDF_FILEWRITE, u32) -> FPDF_BOOL,
    pub FPDFBitmap_CreateEx: unsafe extern "system" fn(c_int, c_int, c_int, *mut c_void, c_int) -> FPDF_BITMAP,
    pub FPDFBitmap_FillRect: unsafe extern "system" fn(FPDF_BITMAP, c_int, c_int, c_int, c_int, c_ulong) -> FPDF_BOOL,
    pub FPDFBitmap_Destroy: unsafe extern "system" fn(FPDF_BITMAP),
    pub FPDF_RenderPageBitmap: unsafe extern "system" fn(FPDF_BITMAP, FPDF_PAGE, c_int, c_int, c_int, c_int, c_int, c_int),
    pub FPDF_RenderPage: unsafe extern "system" fn(HDC, FPDF_PAGE, c_int, c_int, c_int, c_int, c_int, c_int),
}

static API: OnceLock<Api> = OnceLock::new();

/// Loads pdfium.dll from `dir`, resolves every function, and initializes the library.
pub fn load(dir: &Path) -> Result<(), String> {
    let path = dir.join("pdfium.dll");
    let dll = unsafe { LoadLibraryW(&HSTRING::from(path.as_os_str())) }
        .map_err(|e| format!("Could not load {}: {e}", path.display()))?;
    macro_rules! sym {
        ($name:literal) => {{
            let f = unsafe { GetProcAddress(dll, s!($name)) }
                .ok_or_else(|| format!("pdfium.dll is missing {}", $name))?;
            unsafe { std::mem::transmute::<unsafe extern "system" fn() -> isize, _>(f) }
        }};
    }
    let api = Api {
        FPDF_InitLibrary: sym!("FPDF_InitLibrary"),
        FPDF_GetLastError: sym!("FPDF_GetLastError"),
        FPDF_LoadCustomDocument: sym!("FPDF_LoadCustomDocument"),
        FPDF_LoadMemDocument64: sym!("FPDF_LoadMemDocument64"),
        FPDF_CreateNewDocument: sym!("FPDF_CreateNewDocument"),
        FPDF_CloseDocument: sym!("FPDF_CloseDocument"),
        FPDF_GetPageCount: sym!("FPDF_GetPageCount"),
        FPDF_GetPageSizeByIndexF: sym!("FPDF_GetPageSizeByIndexF"),
        FPDF_LoadPage: sym!("FPDF_LoadPage"),
        FPDF_ClosePage: sym!("FPDF_ClosePage"),
        FPDFPage_New: sym!("FPDFPage_New"),
        FPDFPage_Delete: sym!("FPDFPage_Delete"),
        FPDF_ImportPagesByIndex: sym!("FPDF_ImportPagesByIndex"),
        FPDF_MovePages: sym!("FPDF_MovePages"),
        FPDF_SaveAsCopy: sym!("FPDF_SaveAsCopy"),
        FPDFBitmap_CreateEx: sym!("FPDFBitmap_CreateEx"),
        FPDFBitmap_FillRect: sym!("FPDFBitmap_FillRect"),
        FPDFBitmap_Destroy: sym!("FPDFBitmap_Destroy"),
        FPDF_RenderPageBitmap: sym!("FPDF_RenderPageBitmap"),
        FPDF_RenderPage: sym!("FPDF_RenderPage"),
    };
    unsafe { (api.FPDF_InitLibrary)() };
    let _ = API.set(api);
    Ok(())
}

pub fn api() -> &'static Api {
    API.get().expect("pdfium::load() must run first")
}

/// Human-readable FPDF_GetLastError.
pub fn last_error() -> &'static str {
    match unsafe { (api().FPDF_GetLastError)() } {
        2 => "file not found or could not be read",
        3 => "not a PDF, or the file is damaged",
        4 => "password-protected PDFs are not supported yet",
        5 => "unsupported security scheme",
        6 => "page not found or content error",
        _ => "unknown error",
    }
}

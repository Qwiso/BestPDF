//! Raw PDFium C API, resolved at runtime from pdfium.dll next to the exe.
//! Only the functions Best PDF uses; document.rs and stamp.rs wrap them safely.
#![allow(non_snake_case, non_camel_case_types)]

use std::ffi::{c_char, c_int, c_uint, c_ulong, c_void};
use std::path::Path;
use std::sync::OnceLock;

use windows::core::{s, HSTRING};
use windows::Win32::Graphics::Gdi::HDC;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

pub type FPDF_DOCUMENT = *mut c_void;
pub type FPDF_PAGE = *mut c_void;
pub type FPDF_BITMAP = *mut c_void;
pub type FPDF_ANNOTATION = *mut c_void;
pub type FPDF_PAGEOBJECT = *mut c_void;
pub type FPDF_FONT = *mut c_void;
pub type FPDF_TEXTPAGE = *mut c_void;
pub type FPDF_SCHHANDLE = *mut c_void;
pub type FPDF_BOOL = c_int;
/// NUL-terminated UTF-16.
pub type FPDF_WIDESTRING = *const u16;

pub const FPDF_ANNOT: c_int = 0x01;
pub const FPDF_PRINTING: c_int = 0x800;
pub const FPDFBITMAP_BGRX: c_int = 3; // Gray = 1, BGR = 2, BGRx = 3, BGRA = 4
pub const FPDFBITMAP_BGRA: c_int = 4;
pub const FPDF_ANNOT_STAMP: c_int = 13;
pub const FPDF_ANNOT_FLAG_PRINT: c_int = 1 << 2;
pub const FPDF_PAGEOBJ_IMAGE: c_int = 3;
pub const FPDF_FONT_TRUETYPE: c_int = 2;
pub const FPDF_FILLMODE_ALTERNATE: c_int = 1;
pub const FPDF_MATCHCASE: c_ulong = 1;
/// FPDF_SaveAsCopy: write a complete new file (required after page edits).
pub const FPDF_NO_INCREMENTAL: u32 = 2;

#[repr(C)]
pub struct FS_SIZEF {
    pub width: f32,
    pub height: f32,
}

#[repr(C)]
#[derive(Default)]
pub struct FS_RECTF {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

#[repr(C)]
pub struct FS_MATRIX {
    pub a: f32,
    pub b: f32,
    pub c: f32,
    pub d: f32,
    pub e: f32,
    pub f: f32,
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
    pub FPDF_DeviceToPage: unsafe extern "system" fn(FPDF_PAGE, c_int, c_int, c_int, c_int, c_int, c_int, c_int, *mut f64, *mut f64) -> FPDF_BOOL,
    pub FPDF_PageToDevice: unsafe extern "system" fn(FPDF_PAGE, c_int, c_int, c_int, c_int, c_int, f64, f64, *mut c_int, *mut c_int) -> FPDF_BOOL,
    pub FPDFBitmap_GetBuffer: unsafe extern "system" fn(FPDF_BITMAP) -> *mut c_void,
    pub FPDFBitmap_GetStride: unsafe extern "system" fn(FPDF_BITMAP) -> c_int,
    pub FPDFBitmap_GetWidth: unsafe extern "system" fn(FPDF_BITMAP) -> c_int,
    pub FPDFBitmap_GetHeight: unsafe extern "system" fn(FPDF_BITMAP) -> c_int,
    pub FPDFBitmap_GetFormat: unsafe extern "system" fn(FPDF_BITMAP) -> c_int,
    // annotations
    pub FPDFPage_GetAnnotCount: unsafe extern "system" fn(FPDF_PAGE) -> c_int,
    pub FPDFPage_GetAnnot: unsafe extern "system" fn(FPDF_PAGE, c_int) -> FPDF_ANNOTATION,
    pub FPDFPage_CreateAnnot: unsafe extern "system" fn(FPDF_PAGE, c_int) -> FPDF_ANNOTATION,
    pub FPDFPage_RemoveAnnot: unsafe extern "system" fn(FPDF_PAGE, c_int) -> FPDF_BOOL,
    pub FPDFPage_CloseAnnot: unsafe extern "system" fn(FPDF_ANNOTATION),
    pub FPDFAnnot_SetRect: unsafe extern "system" fn(FPDF_ANNOTATION, *const FS_RECTF) -> FPDF_BOOL,
    pub FPDFAnnot_GetRect: unsafe extern "system" fn(FPDF_ANNOTATION, *mut FS_RECTF) -> FPDF_BOOL,
    pub FPDFAnnot_SetFlags: unsafe extern "system" fn(FPDF_ANNOTATION, c_int) -> FPDF_BOOL,
    pub FPDFAnnot_SetStringValue: unsafe extern "system" fn(FPDF_ANNOTATION, *const c_char, FPDF_WIDESTRING) -> FPDF_BOOL,
    /// Returns the byte length needed, including the UTF-16 NUL.
    pub FPDFAnnot_GetStringValue: unsafe extern "system" fn(FPDF_ANNOTATION, *const c_char, *mut u16, c_ulong) -> c_ulong,
    pub FPDFAnnot_AppendObject: unsafe extern "system" fn(FPDF_ANNOTATION, FPDF_PAGEOBJECT) -> FPDF_BOOL,
    pub FPDFAnnot_GetObjectCount: unsafe extern "system" fn(FPDF_ANNOTATION) -> c_int,
    pub FPDFAnnot_GetObject: unsafe extern "system" fn(FPDF_ANNOTATION, c_int) -> FPDF_PAGEOBJECT,
    // page objects
    pub FPDFPage_InsertObject: unsafe extern "system" fn(FPDF_PAGE, FPDF_PAGEOBJECT),
    pub FPDFPage_GenerateContent: unsafe extern "system" fn(FPDF_PAGE) -> FPDF_BOOL,
    pub FPDFPageObj_GetType: unsafe extern "system" fn(FPDF_PAGEOBJECT) -> c_int,
    pub FPDFPageObj_SetMatrix: unsafe extern "system" fn(FPDF_PAGEOBJECT, *const FS_MATRIX) -> FPDF_BOOL,
    pub FPDFPageObj_Transform: unsafe extern "system" fn(FPDF_PAGEOBJECT, f64, f64, f64, f64, f64, f64),
    pub FPDFPageObj_SetFillColor: unsafe extern "system" fn(FPDF_PAGEOBJECT, c_uint, c_uint, c_uint, c_uint) -> FPDF_BOOL,
    pub FPDFPageObj_SetBlendMode: unsafe extern "system" fn(FPDF_PAGEOBJECT, *const c_char),
    pub FPDFPageObj_CreateNewRect: unsafe extern "system" fn(f32, f32, f32, f32) -> FPDF_PAGEOBJECT,
    pub FPDFPath_SetDrawMode: unsafe extern "system" fn(FPDF_PAGEOBJECT, c_int, FPDF_BOOL) -> FPDF_BOOL,
    pub FPDFText_LoadStandardFont: unsafe extern "system" fn(FPDF_DOCUMENT, *const c_char) -> FPDF_FONT,
    pub FPDFText_LoadFont: unsafe extern "system" fn(FPDF_DOCUMENT, *const u8, u32, c_int, FPDF_BOOL) -> FPDF_FONT,
    pub FPDFFont_Close: unsafe extern "system" fn(FPDF_FONT),
    pub FPDFPageObj_CreateTextObj: unsafe extern "system" fn(FPDF_DOCUMENT, FPDF_FONT, f32) -> FPDF_PAGEOBJECT,
    pub FPDFText_SetText: unsafe extern "system" fn(FPDF_PAGEOBJECT, FPDF_WIDESTRING) -> FPDF_BOOL,
    pub FPDFPageObj_NewImageObj: unsafe extern "system" fn(FPDF_DOCUMENT) -> FPDF_PAGEOBJECT,
    pub FPDFImageObj_SetBitmap: unsafe extern "system" fn(*mut FPDF_PAGE, c_int, FPDF_PAGEOBJECT, FPDF_BITMAP) -> FPDF_BOOL,
    pub FPDFImageObj_SetMatrix: unsafe extern "system" fn(FPDF_PAGEOBJECT, f64, f64, f64, f64, f64, f64) -> FPDF_BOOL,
    pub FPDFImageObj_GetImagePixelSize: unsafe extern "system" fn(FPDF_PAGEOBJECT, *mut c_uint, *mut c_uint) -> FPDF_BOOL,
    pub FPDFImageObj_GetRenderedBitmap: unsafe extern "system" fn(FPDF_DOCUMENT, FPDF_PAGE, FPDF_PAGEOBJECT) -> FPDF_BITMAP,
    // rotation
    pub FPDFPage_GetRotation: unsafe extern "system" fn(FPDF_PAGE) -> c_int,
    pub FPDFPage_SetRotation: unsafe extern "system" fn(FPDF_PAGE, c_int),
    // text search
    pub FPDFText_LoadPage: unsafe extern "system" fn(FPDF_PAGE) -> FPDF_TEXTPAGE,
    pub FPDFText_ClosePage: unsafe extern "system" fn(FPDF_TEXTPAGE),
    pub FPDFText_FindStart: unsafe extern "system" fn(FPDF_TEXTPAGE, FPDF_WIDESTRING, c_ulong, c_int) -> FPDF_SCHHANDLE,
    pub FPDFText_FindNext: unsafe extern "system" fn(FPDF_SCHHANDLE) -> FPDF_BOOL,
    pub FPDFText_GetSchResultIndex: unsafe extern "system" fn(FPDF_SCHHANDLE) -> c_int,
    pub FPDFText_GetSchCount: unsafe extern "system" fn(FPDF_SCHHANDLE) -> c_int,
    pub FPDFText_FindClose: unsafe extern "system" fn(FPDF_SCHHANDLE),
    pub FPDFText_CountRects: unsafe extern "system" fn(FPDF_TEXTPAGE, c_int, c_int) -> c_int,
    pub FPDFText_GetRect: unsafe extern "system" fn(FPDF_TEXTPAGE, c_int, *mut f64, *mut f64, *mut f64, *mut f64) -> FPDF_BOOL,
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
        FPDF_DeviceToPage: sym!("FPDF_DeviceToPage"),
        FPDF_PageToDevice: sym!("FPDF_PageToDevice"),
        FPDFBitmap_GetBuffer: sym!("FPDFBitmap_GetBuffer"),
        FPDFBitmap_GetStride: sym!("FPDFBitmap_GetStride"),
        FPDFBitmap_GetWidth: sym!("FPDFBitmap_GetWidth"),
        FPDFBitmap_GetHeight: sym!("FPDFBitmap_GetHeight"),
        FPDFBitmap_GetFormat: sym!("FPDFBitmap_GetFormat"),
        FPDFPage_GetAnnotCount: sym!("FPDFPage_GetAnnotCount"),
        FPDFPage_GetAnnot: sym!("FPDFPage_GetAnnot"),
        FPDFPage_CreateAnnot: sym!("FPDFPage_CreateAnnot"),
        FPDFPage_RemoveAnnot: sym!("FPDFPage_RemoveAnnot"),
        FPDFPage_CloseAnnot: sym!("FPDFPage_CloseAnnot"),
        FPDFAnnot_SetRect: sym!("FPDFAnnot_SetRect"),
        FPDFAnnot_GetRect: sym!("FPDFAnnot_GetRect"),
        FPDFAnnot_SetFlags: sym!("FPDFAnnot_SetFlags"),
        FPDFAnnot_SetStringValue: sym!("FPDFAnnot_SetStringValue"),
        FPDFAnnot_GetStringValue: sym!("FPDFAnnot_GetStringValue"),
        FPDFAnnot_AppendObject: sym!("FPDFAnnot_AppendObject"),
        FPDFAnnot_GetObjectCount: sym!("FPDFAnnot_GetObjectCount"),
        FPDFAnnot_GetObject: sym!("FPDFAnnot_GetObject"),
        FPDFPage_InsertObject: sym!("FPDFPage_InsertObject"),
        FPDFPage_GenerateContent: sym!("FPDFPage_GenerateContent"),
        FPDFPageObj_GetType: sym!("FPDFPageObj_GetType"),
        FPDFPageObj_SetMatrix: sym!("FPDFPageObj_SetMatrix"),
        FPDFPageObj_Transform: sym!("FPDFPageObj_Transform"),
        FPDFPageObj_SetFillColor: sym!("FPDFPageObj_SetFillColor"),
        FPDFPageObj_SetBlendMode: sym!("FPDFPageObj_SetBlendMode"),
        FPDFPageObj_CreateNewRect: sym!("FPDFPageObj_CreateNewRect"),
        FPDFPath_SetDrawMode: sym!("FPDFPath_SetDrawMode"),
        FPDFText_LoadStandardFont: sym!("FPDFText_LoadStandardFont"),
        FPDFText_LoadFont: sym!("FPDFText_LoadFont"),
        FPDFFont_Close: sym!("FPDFFont_Close"),
        FPDFPageObj_CreateTextObj: sym!("FPDFPageObj_CreateTextObj"),
        FPDFText_SetText: sym!("FPDFText_SetText"),
        FPDFPageObj_NewImageObj: sym!("FPDFPageObj_NewImageObj"),
        FPDFImageObj_SetBitmap: sym!("FPDFImageObj_SetBitmap"),
        FPDFImageObj_SetMatrix: sym!("FPDFImageObj_SetMatrix"),
        FPDFImageObj_GetImagePixelSize: sym!("FPDFImageObj_GetImagePixelSize"),
        FPDFImageObj_GetRenderedBitmap: sym!("FPDFImageObj_GetRenderedBitmap"),
        FPDFPage_GetRotation: sym!("FPDFPage_GetRotation"),
        FPDFPage_SetRotation: sym!("FPDFPage_SetRotation"),
        FPDFText_LoadPage: sym!("FPDFText_LoadPage"),
        FPDFText_ClosePage: sym!("FPDFText_ClosePage"),
        FPDFText_FindStart: sym!("FPDFText_FindStart"),
        FPDFText_FindNext: sym!("FPDFText_FindNext"),
        FPDFText_GetSchResultIndex: sym!("FPDFText_GetSchResultIndex"),
        FPDFText_GetSchCount: sym!("FPDFText_GetSchCount"),
        FPDFText_FindClose: sym!("FPDFText_FindClose"),
        FPDFText_CountRects: sym!("FPDFText_CountRects"),
        FPDFText_GetRect: sym!("FPDFText_GetRect"),
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

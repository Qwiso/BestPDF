//! Native Windows Open / Save As dialogs (IFileOpenDialog / IFileSaveDialog).

use std::path::PathBuf;

use windows::core::{w, Interface, HSTRING};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_INPROC_SERVER};
use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
use windows::Win32::UI::Shell::{
    FileOpenDialog, FileSaveDialog, IFileDialog, IFileOpenDialog, IFileSaveDialog, IShellItem,
    FOS_ALLOWMULTISELECT, FOS_OVERWRITEPROMPT, SIGDN_FILESYSPATH,
};

const FILTERS: [COMDLG_FILTERSPEC; 2] = [
    COMDLG_FILTERSPEC { pszName: w!("PDF documents"), pszSpec: w!("*.pdf") },
    COMDLG_FILTERSPEC { pszName: w!("All files"), pszSpec: w!("*.*") },
];

/// Shows the Open dialog (multi-select). Empty if the user cancels.
pub fn pick_pdfs(owner: HWND) -> Vec<PathBuf> {
    unsafe {
        let pick = || -> Option<Vec<PathBuf>> {
            let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
            dialog.SetTitle(w!("Open PDF")).ok()?;
            dialog.SetOptions(dialog.GetOptions().ok()? | FOS_ALLOWMULTISELECT).ok()?;
            dialog.SetFileTypes(&FILTERS).ok()?;
            dialog.Show(Some(owner)).ok()?; // Err when cancelled
            let items = dialog.GetResults().ok()?;
            Some((0..items.GetCount().ok()?).filter_map(|i| path_of(&items.GetItemAt(i).ok()?)).collect())
        };
        pick().unwrap_or_default()
    }
}

/// Shows the Open dialog for one picture (signature import).
pub fn pick_image(owner: HWND) -> Option<PathBuf> {
    const IMAGES: [COMDLG_FILTERSPEC; 2] = [
        COMDLG_FILTERSPEC { pszName: w!("Pictures"), pszSpec: w!("*.png;*.jpg;*.jpeg;*.bmp;*.gif;*.tif;*.tiff;*.webp") },
        COMDLG_FILTERSPEC { pszName: w!("All files"), pszSpec: w!("*.*") },
    ];
    unsafe {
        let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        dialog.SetTitle(w!("Import Signature Picture")).ok()?;
        dialog.SetFileTypes(&IMAGES).ok()?;
        dialog.Show(Some(owner)).ok()?; // Err when cancelled
        path_of(&dialog.GetResult().ok()?)
    }
}

/// Shows the Save As dialog, pre-filled with `suggested_name`.
pub fn save_pdf(owner: HWND, suggested_name: &str) -> Option<PathBuf> {
    unsafe {
        let dialog: IFileSaveDialog = CoCreateInstance(&FileSaveDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        dialog.SetDefaultExtension(w!("pdf")).ok()?;
        dialog.SetFileName(&HSTRING::from(suggested_name)).ok()?;
        dialog.SetOptions(dialog.GetOptions().ok()? | FOS_OVERWRITEPROMPT).ok()?;
        run(&dialog.cast().ok()?, owner)
    }
}

unsafe fn run(dialog: &IFileDialog, owner: HWND) -> Option<PathBuf> {
    dialog.SetFileTypes(&FILTERS).ok()?;
    dialog.Show(Some(owner)).ok()?; // Err when cancelled
    path_of(&dialog.GetResult().ok()?)
}

unsafe fn path_of(item: &IShellItem) -> Option<PathBuf> {
    let raw = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
    let path = raw.to_string().ok().map(PathBuf::from);
    CoTaskMemFree(Some(raw.0 as *const _));
    path
}

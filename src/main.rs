// Release builds are GUI-only; debug builds keep a console for logs.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod dialog;
mod document;
mod gdi;
mod pdfium;
mod print;
mod tab;
mod theme;
mod ui;

use std::cell::{Cell, RefCell};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use windows::core::{w, HSTRING, PCSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE};
use windows::Win32::System::DataExchange::COPYDATASTRUCT;
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};
use windows::Win32::UI::Controls::WM_MOUSELEAVE;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::{DragAcceptFiles, DragFinish, DragQueryFileW, HDROP};
use windows::Win32::UI::WindowsAndMessaging::*;

use document::Document;
use ui::{Action, App};

const CLASS: windows::core::PCWSTR = w!("BestPdfWindow");
/// Icon resource id from assets/app.rc (MAKEINTRESOURCE(1)).
const APP_ICON: windows::core::PCWSTR = windows::core::PCWSTR(1 as *const u16);
/// WM_COPYDATA tag: a second launch hands its files to the running window.
const COPYDATA_OPEN: usize = 0xBE57_0001;

const ID_NEW: u16 = 100;
const ID_OPEN: u16 = 101;
const ID_CLOSE: u16 = 102;
const ID_SAVE: u16 = 103;
const ID_SAVE_AS: u16 = 104;
const ID_PRINT: u16 = 105;
const ID_EXIT: u16 = 106;
const ID_NEXT_TAB: u16 = 107;
const ID_PREV_TAB: u16 = 108;
const ID_NEW_TAB: u16 = 109;
/// Timer that drives page drags (spring-loaded tabs, auto-scroll).
const DRAG_TIMER: usize = 1;
/// Commands that need a document in the active tab; greyed out otherwise.
const DOC_COMMANDS: [u16; 3] = [ID_SAVE, ID_SAVE_AS, ID_PRINT];

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
    static FILE_MENU: Cell<HMENU> = Cell::new(HMENU::default());
    static TRACKING_LEAVE: Cell<bool> = const { Cell::new(false) };
    static LAST_TITLE: RefCell<String> = const { RefCell::new(String::new()) };
}

/// Runs `f` against the app. Returns None if it is already borrowed — Win32
/// re-enters the window procedure from inside some calls (modal dialogs etc.).
fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|a| a.try_borrow_mut().ok()?.as_mut().map(f))
}

fn main() -> windows::core::Result<()> {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        if forward_to_running_instance(&args) {
            return Ok(());
        }
        enable_dark_menus();
        CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE).ok()?;

        let instance = GetModuleHandleW(None)?;
        let icon = |size_metric| {
            let size = GetSystemMetrics(size_metric);
            LoadImageW(Some(instance.into()), APP_ICON, IMAGE_ICON, size, size, LR_DEFAULTCOLOR)
                .map(|h| HICON(h.0))
                .unwrap_or_default()
        };
        RegisterClassExW(&WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wnd_proc),
            hInstance: instance.into(),
            hIcon: icon(SM_CXICON),
            hIconSm: icon(SM_CXSMICON),
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            hbrBackground: CreateSolidBrush(theme::EDITOR_BG),
            lpszClassName: CLASS,
            ..Default::default()
        });

        let screen_h = GetSystemMetrics(SM_CYSCREEN);
        let (h, w) = (screen_h * 85 / 100, screen_h * 85 / 100);
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            CLASS,
            w!("Best PDF"),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT, CW_USEDEFAULT, w, h,
            None, None, Some(instance.into()), None,
        )?;
        style_caption(hwnd);
        FILE_MENU.set(create_file_menu()?);
        let accelerators = create_accelerators()?;

        APP.with(|a| *a.borrow_mut() = Some(App::new(GetDpiForWindow(hwnd))));
        let mut client = RECT::default();
        let _ = GetClientRect(hwnd, &mut client);
        with_app(|a| a.resize(client.right, client.bottom));
        DragAcceptFiles(hwnd, true);
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = UpdateWindow(hwnd);

        // Load PDFium after the window is on screen so startup feels instant.
        if let Err(e) = document::init() {
            error_box(hwnd, &e);
            return Ok(());
        }
        open_paths(hwnd, args.into_iter().map(PathBuf::from).collect());

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if TranslateAcceleratorW(hwnd, accelerators, &msg) == 0 {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
    Ok(())
}

/// If Best PDF is already running and files were passed, open them there
/// (as new tabs) instead of starting a second window.
fn forward_to_running_instance(args: &[OsString]) -> bool {
    if args.is_empty() {
        return false;
    }
    let Ok(existing) = (unsafe { FindWindowW(CLASS, None) }) else { return false };
    // The running instance has a different working directory: send absolute paths.
    let joined: Vec<u16> = args
        .iter()
        .map(|a| std::path::absolute(a).unwrap_or_else(|_| PathBuf::from(a)).to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("\n")
        .encode_utf16()
        .collect();
    let data = COPYDATASTRUCT { dwData: COPYDATA_OPEN, cbData: (joined.len() * 2) as u32, lpData: joined.as_ptr() as *mut _ };
    unsafe {
        if IsIconic(existing).as_bool() {
            let _ = ShowWindow(existing, SW_RESTORE);
        }
        let _ = SetForegroundWindow(existing);
        SendMessageW(existing, WM_COPYDATA, None, Some(LPARAM(&data as *const _ as isize)));
    }
    true
}

/// Dark popup menus. uxtheme's SetPreferredAppMode (#135) / FlushMenuThemes (#136)
/// are undocumented but stable since Windows 10 1903 (used by Explorer, Notepad++);
/// if they're missing we simply get light menus.
fn enable_dark_menus() {
    unsafe {
        let Ok(uxtheme) = LoadLibraryW(w!("uxtheme.dll")) else { return };
        if let Some(f) = GetProcAddress(uxtheme, PCSTR(135 as *const u8)) {
            let set_preferred_app_mode: unsafe extern "system" fn(i32) -> i32 = std::mem::transmute(f);
            set_preferred_app_mode(2); // ForceDark
        }
        if let Some(f) = GetProcAddress(uxtheme, PCSTR(136 as *const u8)) {
            let flush_menu_themes: unsafe extern "system" fn() = std::mem::transmute(f);
            flush_menu_themes();
        }
    }
}

/// Dark title bar tinted to match the menu strip (caption colors need Windows 11).
fn style_caption(hwnd: HWND) {
    let set = |attr: DWMWINDOWATTRIBUTE, value: u32| unsafe {
        let _ = DwmSetWindowAttribute(hwnd, attr, &value as *const u32 as *const _, 4);
    };
    set(DWMWA_USE_IMMERSIVE_DARK_MODE, 1);
    set(DWMWA_CAPTION_COLOR, theme::CHROME_BG.0);
    set(DWMWA_TEXT_COLOR, theme::TEXT.0);
    set(DWMWA_BORDER_COLOR, theme::BORDER.0);
}

fn create_file_menu() -> windows::core::Result<HMENU> {
    unsafe {
        let file = CreatePopupMenu()?;
        AppendMenuW(file, MF_STRING, ID_NEW_TAB as usize, w!("New &Tab\tCtrl+T"))?;
        AppendMenuW(file, MF_STRING, ID_NEW as usize, w!("&New Document\tCtrl+N"))?;
        AppendMenuW(file, MF_STRING, ID_OPEN as usize, w!("&Open…\tCtrl+O"))?;
        AppendMenuW(file, MF_STRING, ID_CLOSE as usize, w!("&Close Tab\tCtrl+W"))?;
        AppendMenuW(file, MF_SEPARATOR, 0, None)?;
        AppendMenuW(file, MF_STRING, ID_SAVE as usize, w!("&Save\tCtrl+S"))?;
        AppendMenuW(file, MF_STRING, ID_SAVE_AS as usize, w!("Save &As…\tCtrl+Shift+S"))?;
        AppendMenuW(file, MF_SEPARATOR, 0, None)?;
        AppendMenuW(file, MF_STRING, ID_PRINT as usize, w!("&Print…\tCtrl+P"))?;
        AppendMenuW(file, MF_SEPARATOR, 0, None)?;
        AppendMenuW(file, MF_STRING, ID_EXIT as usize, w!("E&xit"))?;
        Ok(file)
    }
}

fn create_accelerators() -> windows::core::Result<HACCEL> {
    let key = |k: u16, shift: bool, cmd: u16| ACCEL {
        fVirt: FVIRTKEY | FCONTROL | if shift { FSHIFT } else { ACCEL_VIRT_FLAGS(0) },
        key: k,
        cmd,
    };
    unsafe {
        CreateAcceleratorTableW(&[
            key(b'N' as u16, false, ID_NEW),
            key(b'T' as u16, false, ID_NEW_TAB),
            key(b'O' as u16, false, ID_OPEN),
            key(b'W' as u16, false, ID_CLOSE),
            key(VK_F4.0, false, ID_CLOSE),
            key(b'S' as u16, false, ID_SAVE),
            key(b'S' as u16, true, ID_SAVE_AS),
            key(b'P' as u16, false, ID_PRINT),
            key(VK_TAB.0, false, ID_NEXT_TAB),
            key(VK_TAB.0, true, ID_PREV_TAB),
            key(VK_NEXT.0, false, ID_NEXT_TAB),
            key(VK_PRIOR.0, false, ID_PREV_TAB),
        ])
    }
}

fn lparam_point(lparam: LPARAM) -> (i32, i32) {
    ((lparam.0 & 0xFFFF) as i16 as i32, ((lparam.0 >> 16) & 0xFFFF) as i16 as i32)
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            with_app(|a| a.paint(hdc));
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1), // we paint every pixel; skipping erase avoids flicker
        WM_SIZE => {
            let (w, h) = lparam_point(lparam);
            with_app(|a| a.resize(w, h));
            LRESULT(0)
        }
        WM_ENTERSIZEMOVE => {
            with_app(|a| a.live_resize = true);
            LRESULT(0)
        }
        WM_EXITSIZEMOVE => {
            with_app(|a| a.live_resize = false);
            perform(hwnd, Some(Action::Redraw));
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            if !TRACKING_LEAVE.get() {
                let mut tme = TRACKMOUSEEVENT { cbSize: size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: hwnd, dwHoverTime: 0 };
                let _ = TrackMouseEvent(&mut tme);
                TRACKING_LEAVE.set(true);
            }
            let (x, y) = lparam_point(lparam);
            perform(hwnd, with_app(|a| a.mouse_move(x, y)));
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            TRACKING_LEAVE.set(false);
            perform(hwnd, with_app(|a| a.mouse_leave()));
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let (x, y) = lparam_point(lparam);
            perform(hwnd, with_app(|a| a.left_down(x, y)));
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            perform(hwnd, with_app(|a| a.left_up()));
            LRESULT(0)
        }
        WM_MBUTTONDOWN => {
            let (x, y) = lparam_point(lparam);
            perform(hwnd, with_app(|a| a.middle_down(x, y)));
            LRESULT(0)
        }
        WM_CAPTURECHANGED => {
            with_app(|a| a.capture_lost());
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            let delta = ((wparam.0 >> 16) & 0xFFFF) as i16 as i32;
            let (sx, sy) = lparam_point(lparam);
            let mut pt = POINT { x: sx, y: sy };
            let _ = ScreenToClient(hwnd, &mut pt);
            perform(hwnd, with_app(|a| a.wheel(pt.x, pt.y, delta)));
            LRESULT(0)
        }
        WM_KEYDOWN => {
            perform(hwnd, with_app(|a| a.key(VIRTUAL_KEY(wparam.0 as u16))));
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == DRAG_TIMER => {
            perform(hwnd, with_app(|a| a.tick()));
            LRESULT(0)
        }
        WM_CLOSE => {
            // Ask about every tab with unsaved changes before quitting.
            while let Some(i) = with_app(|a| a.first_unsaved()).flatten() {
                if !close_tab(hwnd, i) {
                    return LRESULT(0); // cancelled
                }
            }
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_SYSCHAR if matches!(wparam.0 as u8, b'f' | b'F') => {
            if let Some((x, y)) = with_app(|a| a.file_menu_point()) {
                show_file_menu(hwnd, x, y);
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            on_command(hwnd, (wparam.0 & 0xFFFF) as u16);
            LRESULT(0)
        }
        WM_INITMENUPOPUP => {
            let menu = HMENU(wparam.0 as *mut _);
            let (has_tabs, has_doc) = with_app(|a| (a.has_tabs(), a.active_tab().is_some())).unwrap_or_default();
            let state = |on: bool| MF_BYCOMMAND | if on { MF_ENABLED } else { MF_GRAYED };
            let _ = EnableMenuItem(menu, ID_CLOSE as u32, state(has_tabs));
            for id in DOC_COMMANDS {
                let _ = EnableMenuItem(menu, id as u32, state(has_doc));
            }
            LRESULT(0)
        }
        WM_DROPFILES => {
            let drop = HDROP(wparam.0 as *mut _);
            let count = DragQueryFileW(drop, u32::MAX, None);
            let paths = (0..count)
                .filter_map(|i| {
                    let mut buf = [0u16; 1024];
                    let len = DragQueryFileW(drop, i, Some(&mut buf)) as usize;
                    (len > 0).then(|| PathBuf::from(String::from_utf16_lossy(&buf[..len])))
                })
                .collect();
            DragFinish(drop);
            open_paths(hwnd, paths);
            LRESULT(0)
        }
        WM_COPYDATA => {
            let data = &*(lparam.0 as *const COPYDATASTRUCT);
            if data.dwData != COPYDATA_OPEN || data.lpData.is_null() {
                return LRESULT(0);
            }
            let text = std::slice::from_raw_parts(data.lpData as *const u16, data.cbData as usize / 2);
            let paths = String::from_utf16_lossy(text).split('\n').map(PathBuf::from).collect();
            open_paths(hwnd, paths);
            LRESULT(1)
        }
        WM_DPICHANGED => {
            let dpi = ((wparam.0 >> 16) & 0xFFFF) as u32;
            with_app(|a| a.set_dpi(dpi));
            let r = &*(lparam.0 as *const RECT);
            let _ = SetWindowPos(hwnd, None, r.left, r.top, r.right - r.left, r.bottom - r.top,
                SWP_NOZORDER | SWP_NOACTIVATE);
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Carries out what an input handler asked for, outside the app borrow.
fn perform(hwnd: HWND, action: Option<Action>) {
    unsafe {
        match action {
            None | Some(Action::None) => return,
            Some(Action::Redraw) => {}
            Some(Action::Capture) => {
                SetCapture(hwnd);
                SetTimer(Some(hwnd), DRAG_TIMER, 30, None);
            }
            Some(Action::Release(error)) => {
                let _ = KillTimer(Some(hwnd), DRAG_TIMER);
                let _ = ReleaseCapture();
                refresh(hwnd);
                if let Some(e) = error {
                    error_box(hwnd, &e);
                }
                return;
            }
            Some(Action::FileMenu(x, y)) => return show_file_menu(hwnd, x, y),
            Some(Action::CloseTab(i)) => {
                close_tab(hwnd, i);
                let mut pt = POINT::default();
                let _ = GetCursorPos(&mut pt);
                let _ = ScreenToClient(hwnd, &mut pt);
                with_app(|a| a.rehover(pt.x, pt.y));
            }
        }
    }
    refresh(hwnd);
}

/// Closes tab `i`, first offering to save unsaved changes.
/// Returns false if the user cancelled (or saving failed).
fn close_tab(hwnd: HWND, i: usize) -> bool {
    if let Some(name) = with_app(|a| a.unsaved_name(i)).flatten() {
        with_app(|a| a.activate(i)); // Save acts on the active tab
        refresh(hwnd);
        let text = HSTRING::from(format!("Do you want to save the changes you made to {name}?"));
        let choice = unsafe { MessageBoxW(Some(hwnd), &text, w!("Best PDF"), MB_YESNOCANCEL | MB_ICONWARNING) };
        match choice {
            IDYES if !save_active(hwnd) => return false,
            IDYES | IDNO => {}
            _ => return false,
        }
    }
    with_app(|a| a.close_tab(i));
    refresh(hwnd);
    true
}

fn refresh(hwnd: HWND) {
    if let Some(title) = with_app(|a| a.title()) {
        // Only touch the caption when it changes; it's a cross-process repaint.
        let changed = LAST_TITLE.with(|t| {
            let mut t = t.borrow_mut();
            (*t != title).then(|| *t = title.clone()).is_some()
        });
        if changed {
            unsafe { let _ = SetWindowTextW(hwnd, &HSTRING::from(title)); }
        }
    }
    unsafe { let _ = InvalidateRect(Some(hwnd), None, false); }
}

fn show_file_menu(hwnd: HWND, x: i32, y: i32) {
    let mut pt = POINT { x, y };
    unsafe {
        let _ = ClientToScreen(hwnd, &mut pt);
        // Modal: runs its own loop and sends WM_COMMAND on selection.
        let _ = TrackPopupMenu(FILE_MENU.get(), TPM_LEFTALIGN | TPM_TOPALIGN, pt.x, pt.y, None, hwnd, None);
    }
    perform(hwnd, with_app(|a| a.mouse_leave()));
}

fn on_command(hwnd: HWND, id: u16) {
    match id {
        ID_NEW => match Document::new_blank() {
            Ok(doc) => {
                with_app(|a| a.add_document(doc));
                refresh(hwnd);
            }
            Err(e) => error_box(hwnd, &e),
        },
        ID_NEW_TAB => {
            with_app(|a| a.add_welcome());
            refresh(hwnd);
        }
        ID_OPEN => {
            // The dialog runs a modal loop that repaints us, so no borrow is held here.
            let paths = dialog::pick_pdfs(hwnd);
            open_paths(hwnd, paths);
        }
        ID_CLOSE => {
            if let Some(i) = with_app(|a| a.has_tabs().then(|| a.active_index())).flatten() {
                close_tab(hwnd, i);
            }
        }
        ID_SAVE => {
            save_active(hwnd);
        }
        ID_SAVE_AS => {
            prompt_save_as(hwnd);
        }
        ID_PRINT => print_document(hwnd),
        ID_NEXT_TAB | ID_PREV_TAB => {
            with_app(|a| a.cycle_tab(id == ID_NEXT_TAB));
            refresh(hwnd);
        }
        ID_EXIT => unsafe { let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)); },
        _ => {}
    }
}

/// Opens each file in its own tab (or focuses its existing tab).
fn open_paths(hwnd: HWND, paths: Vec<PathBuf>) {
    let mut errors = Vec::new();
    for path in paths {
        if with_app(|a| a.focus_path(&path)) == Some(true) {
            continue;
        }
        match Document::open(&path) {
            Ok(doc) => { with_app(|a| a.add_document(doc)); }
            Err(e) => errors.push(e),
        }
    }
    refresh(hwnd);
    if !errors.is_empty() {
        error_box(hwnd, &errors.join("\n\n"));
    }
}

/// Saves the active tab in place (Save As if untitled). Returns true if saved.
fn save_active(hwnd: HWND) -> bool {
    match with_app(|a| a.active_tab().map(|t| t.doc.path.clone())).flatten() {
        Some(Some(path)) => save_to(hwnd, &path),
        Some(None) => prompt_save_as(hwnd),
        None => false,
    }
}

fn prompt_save_as(hwnd: HWND) -> bool {
    let Some(name) = with_app(|a| a.active_tab().map(|t| t.name())).flatten() else { return false };
    let suggested = if name.to_ascii_lowercase().ends_with(".pdf") { name } else { format!("{name}.pdf") };
    match dialog::save_pdf(hwnd, &suggested) {
        Some(path) => save_to(hwnd, &path),
        None => false,
    }
}

fn save_to(hwnd: HWND, path: &Path) -> bool {
    let result = with_app(|a| {
        a.active_tab_mut().map(|t| {
            let r = t.doc.save_as(path);
            if r.is_ok() {
                t.dirty = false;
            }
            r
        })
    })
    .flatten();
    refresh(hwnd); // tab name may change (Untitled-1 → file name)
    match result {
        Some(Ok(())) => true,
        Some(Err(e)) => {
            error_box(hwnd, &e);
            false
        }
        None => false,
    }
}

fn print_document(hwnd: HWND) {
    let Some((title, pages, bytes)) =
        with_app(|a| a.active_tab().map(|t| (t.name(), t.doc.page_count(), t.doc.to_bytes()))).flatten()
    else {
        return;
    };
    let result = bytes.and_then(|bytes| {
        // Dialog first (modal, re-enters wnd_proc), then spool with a busy cursor.
        let cursor = unsafe { SetCursor(LoadCursorW(None, IDC_WAIT).ok()) };
        let r = print::print(hwnd, &title, bytes, pages);
        unsafe { SetCursor(Some(cursor)); }
        r
    });
    if let Err(e) = result {
        error_box(hwnd, &e);
    }
}

fn error_box(hwnd: HWND, text: &str) {
    unsafe {
        MessageBoxW(Some(hwnd), &HSTRING::from(text), w!("Best PDF"), MB_OK | MB_ICONWARNING);
    }
}

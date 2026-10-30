// Release builds are GUI-only; debug builds keep a console for logs.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod dialog;
mod document;
mod gdi;
mod imageio;
mod overlay;
mod pdfium;
mod print;
mod settings;
mod sheet;
mod signature;
mod signpad;
mod stamp;
mod tab;
mod theme;
mod ui;

use std::cell::{Cell, RefCell};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use windows::core::{w, BOOL, HSTRING, PCSTR, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE};
use windows::Win32::System::DataExchange::COPYDATASTRUCT;
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};
use windows::Win32::UI::Controls::*;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::{DragAcceptFiles, DragFinish, DragQueryFileW, HDROP};
use windows::Win32::UI::WindowsAndMessaging::*;

use document::Document;
use settings::OverlaySave;
use ui::{Action, App, Cursor};

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
const ID_SAVE_ASK: u16 = 110;
const ID_SAVE_ANNOTATE: u16 = 111;
const ID_SAVE_FLATTEN: u16 = 112;
const ID_ZOOM_IN: u16 = 113;
const ID_ZOOM_OUT: u16 = 114;
const ID_ZOOM_RESET: u16 = 115;
const ID_UNDO: u16 = 116;
const ID_REDO: u16 = 117;
const ID_FIND: u16 = 118;
const ID_FIND_NEXT: u16 = 119;
const ID_FIND_PREV: u16 = 120;
const ID_ROTATE_CW: u16 = 121;
const ID_ROTATE_CCW: u16 = 122;
const ID_DELETE_PAGE: u16 = 123;
const ID_INSERT_FILE: u16 = 124;
const ID_COMBINE: u16 = 125;
const ID_EXTRACT: u16 = 126;
const ID_SPLIT: u16 = 127;
const ID_EXPORT_IMAGES: u16 = 128;
const ID_SHRINK: u16 = 129;
const ID_GOTO: u16 = 130;
const ID_FIT_WIDTH: u16 = 131;
const ID_FIT_PAGE: u16 = 132;
const ID_TWO_PAGES: u16 = 133;
const ID_SELECT_PAGES: u16 = 134;
/// Timer that drives page drags (spring-loaded tabs, auto-scroll).
const DRAG_TIMER: usize = 1;
/// Commands that need a document in the active tab; greyed out otherwise.
const DOC_COMMANDS: [u16; 22] = [
    ID_SAVE, ID_SAVE_AS, ID_PRINT, ID_FIND, ID_FIND_NEXT, ID_FIND_PREV, ID_ROTATE_CW, ID_ROTATE_CCW, ID_DELETE_PAGE,
    ID_INSERT_FILE, ID_EXTRACT, ID_SPLIT, ID_EXPORT_IMAGES, ID_SHRINK, ID_GOTO, ID_FIT_WIDTH, ID_FIT_PAGE, ID_TWO_PAGES,
    ID_SELECT_PAGES, ID_ZOOM_IN, ID_ZOOM_OUT, ID_ZOOM_RESET,
];

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
    static FILE_MENU: Cell<HMENU> = Cell::new(HMENU::default());
    static EDIT_MENU: Cell<HMENU> = Cell::new(HMENU::default());
    static VIEW_MENU: Cell<HMENU> = Cell::new(HMENU::default());
    static PAGE_MENU: Cell<HMENU> = Cell::new(HMENU::default());
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
            style: CS_HREDRAW | CS_VREDRAW | CS_DBLCLKS,
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
        EDIT_MENU.set(create_edit_menu()?);
        VIEW_MENU.set(create_view_menu()?);
        PAGE_MENU.set(create_page_menu()?);
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
        AppendMenuW(file, MF_STRING, ID_INSERT_FILE as usize, w!("&Insert Pages from File…"))?;
        AppendMenuW(file, MF_STRING, ID_COMBINE as usize, w!("Co&mbine Files into New Document…"))?;
        AppendMenuW(file, MF_STRING, ID_EXTRACT as usize, w!("&Extract Pages…"))?;
        AppendMenuW(file, MF_STRING, ID_SPLIT as usize, w!("Sp&lit into Files…"))?;
        AppendMenuW(file, MF_SEPARATOR, 0, None)?;
        AppendMenuW(file, MF_STRING, ID_SAVE as usize, w!("&Save\tCtrl+S"))?;
        AppendMenuW(file, MF_STRING, ID_SAVE_AS as usize, w!("Save &As…\tCtrl+Shift+S"))?;
        let overlays = CreatePopupMenu()?;
        AppendMenuW(overlays, MF_STRING, ID_SAVE_ASK as usize, w!("&Ask Each Time"))?;
        AppendMenuW(overlays, MF_STRING, ID_SAVE_ANNOTATE as usize, w!("Keep &Editable (Annotations)"))?;
        AppendMenuW(overlays, MF_STRING, ID_SAVE_FLATTEN as usize, w!("&Flatten into Page"))?;
        AppendMenuW(file, MF_POPUP, overlays.0 as usize, w!("&When Saving Text && Signatures"))?;
        AppendMenuW(file, MF_SEPARATOR, 0, None)?;
        AppendMenuW(file, MF_STRING, ID_PRINT as usize, w!("&Print…\tCtrl+P"))?;
        AppendMenuW(file, MF_STRING, ID_EXPORT_IMAGES as usize, w!("Export Pages as &Images…"))?;
        AppendMenuW(file, MF_STRING, ID_SHRINK as usize, w!("Save &Reduced-Size Copy…"))?;
        AppendMenuW(file, MF_SEPARATOR, 0, None)?;
        AppendMenuW(file, MF_STRING, ID_EXIT as usize, w!("E&xit"))?;
        Ok(file)
    }
}

fn create_edit_menu() -> windows::core::Result<HMENU> {
    unsafe {
        let edit = CreatePopupMenu()?;
        AppendMenuW(edit, MF_STRING, ID_UNDO as usize, w!("&Undo\tCtrl+Z"))?;
        AppendMenuW(edit, MF_STRING, ID_REDO as usize, w!("&Redo\tCtrl+Y"))?;
        AppendMenuW(edit, MF_SEPARATOR, 0, None)?;
        AppendMenuW(edit, MF_STRING, ID_FIND as usize, w!("&Find…\tCtrl+F"))?;
        AppendMenuW(edit, MF_STRING, ID_FIND_NEXT as usize, w!("Find &Next\tF3"))?;
        AppendMenuW(edit, MF_STRING, ID_FIND_PREV as usize, w!("Find &Previous\tShift+F3"))?;
        AppendMenuW(edit, MF_SEPARATOR, 0, None)?;
        AppendMenuW(edit, MF_STRING, ID_ROTATE_CW as usize, w!("Rotate Page &Clockwise\tCtrl+R"))?;
        AppendMenuW(edit, MF_STRING, ID_ROTATE_CCW as usize, w!("Rotate Page Counterc&lockwise\tCtrl+Shift+R"))?;
        AppendMenuW(edit, MF_STRING, ID_DELETE_PAGE as usize, w!("&Delete Page"))?;
        AppendMenuW(edit, MF_STRING, ID_EXTRACT as usize, w!("&Extract Pages…"))?;
        AppendMenuW(edit, MF_STRING, ID_SELECT_PAGES as usize, w!("Select &All Pages\tCtrl+A"))?;
        Ok(edit)
    }
}

fn create_view_menu() -> windows::core::Result<HMENU> {
    unsafe {
        let view = CreatePopupMenu()?;
        AppendMenuW(view, MF_STRING, ID_GOTO as usize, w!("&Go to Page…\tCtrl+G"))?;
        AppendMenuW(view, MF_SEPARATOR, 0, None)?;
        AppendMenuW(view, MF_STRING, ID_FIT_WIDTH as usize, w!("Fit &Width\tCtrl+1"))?;
        AppendMenuW(view, MF_STRING, ID_FIT_PAGE as usize, w!("Fit &Page\tCtrl+2"))?;
        AppendMenuW(view, MF_STRING, ID_ZOOM_RESET as usize, w!("&Default Zoom (75%)\tCtrl+0"))?;
        AppendMenuW(view, MF_STRING, ID_ZOOM_IN as usize, w!("Zoom &In\tCtrl+="))?;
        AppendMenuW(view, MF_STRING, ID_ZOOM_OUT as usize, w!("Zoom &Out\tCtrl+-"))?;
        AppendMenuW(view, MF_SEPARATOR, 0, None)?;
        AppendMenuW(view, MF_STRING, ID_TWO_PAGES as usize, w!("&Two Pages Side by Side\tCtrl+3"))?;
        Ok(view)
    }
}

/// Right-click on a page.
fn create_page_menu() -> windows::core::Result<HMENU> {
    unsafe {
        let page = CreatePopupMenu()?;
        AppendMenuW(page, MF_STRING, ID_ROTATE_CW as usize, w!("Rotate &Clockwise\tCtrl+R"))?;
        AppendMenuW(page, MF_STRING, ID_ROTATE_CCW as usize, w!("Rotate Counterc&lockwise\tCtrl+Shift+R"))?;
        AppendMenuW(page, MF_SEPARATOR, 0, None)?;
        AppendMenuW(page, MF_STRING, ID_DELETE_PAGE as usize, w!("&Delete Page"))?;
        AppendMenuW(page, MF_STRING, ID_EXTRACT as usize, w!("&Extract Pages…"))?;
        AppendMenuW(page, MF_SEPARATOR, 0, None)?;
        AppendMenuW(page, MF_STRING, ID_UNDO as usize, w!("&Undo\tCtrl+Z"))?;
        AppendMenuW(page, MF_STRING, ID_REDO as usize, w!("&Redo\tCtrl+Y"))?;
        Ok(page)
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
            key(VK_OEM_PLUS.0, false, ID_ZOOM_IN), // Ctrl+= (and Ctrl++ with Shift)
            key(VK_OEM_PLUS.0, true, ID_ZOOM_IN),
            key(VK_ADD.0, false, ID_ZOOM_IN),
            key(VK_OEM_MINUS.0, false, ID_ZOOM_OUT),
            key(VK_SUBTRACT.0, false, ID_ZOOM_OUT),
            key(b'0' as u16, false, ID_ZOOM_RESET),
            key(VK_NUMPAD0.0, false, ID_ZOOM_RESET),
            key(b'Z' as u16, false, ID_UNDO),
            key(b'Y' as u16, false, ID_REDO),
            key(b'Z' as u16, true, ID_REDO),
            key(b'F' as u16, false, ID_FIND),
            key(b'R' as u16, false, ID_ROTATE_CW),
            key(b'R' as u16, true, ID_ROTATE_CCW),
            key(b'G' as u16, false, ID_GOTO),
            key(b'1' as u16, false, ID_FIT_WIDTH),
            key(b'2' as u16, false, ID_FIT_PAGE),
            key(b'3' as u16, false, ID_TWO_PAGES),
            key(b'A' as u16, false, ID_SELECT_PAGES),
            ACCEL { fVirt: FVIRTKEY, key: VK_F3.0, cmd: ID_FIND_NEXT },
            ACCEL { fVirt: FVIRTKEY | FSHIFT, key: VK_F3.0, cmd: ID_FIND_PREV },
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
            with_app(|a| a.set_mods(wparam.0 & 0x8 != 0, wparam.0 & 0x4 != 0)); // MK_CONTROL, MK_SHIFT
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
            with_app(|a| a.set_mods(wparam.0 & 0x8 != 0, wparam.0 & 0x4 != 0)); // MK_CONTROL, MK_SHIFT
            perform(hwnd, with_app(|a| a.left_down(x, y)));
            LRESULT(0)
        }
        WM_LBUTTONDBLCLK => {
            let (x, y) = lparam_point(lparam);
            perform(hwnd, with_app(|a| a.double_click(x, y)));
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            let (x, y) = lparam_point(lparam);
            if let Some(Some(page)) = with_app(|a| a.page_at_point(x, y)) {
                show_page_menu(hwnd, page, x, y);
            }
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
        WM_MOUSEHWHEEL => {
            let delta = ((wparam.0 >> 16) & 0xFFFF) as i16 as i32;
            perform(hwnd, with_app(|a| a.hwheel(delta)));
            LRESULT(0)
        }
        WM_KEYDOWN => {
            perform(hwnd, with_app(|a| a.key(VIRTUAL_KEY(wparam.0 as u16))));
            LRESULT(0)
        }
        WM_CHAR => {
            perform(hwnd, with_app(|a| a.char_input(wparam.0 as u16)));
            LRESULT(0)
        }
        WM_SETCURSOR if (lparam.0 & 0xFFFF) as u32 == HTCLIENT => {
            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            let _ = ScreenToClient(hwnd, &mut pt);
            let id = match with_app(|a| a.cursor(pt.x, pt.y)) {
                Some(Cursor::Hand) => IDC_HAND,
                Some(Cursor::Move) => IDC_SIZEALL,
                Some(Cursor::SizeNWSE) => IDC_SIZENWSE,
                Some(Cursor::SizeNESW) => IDC_SIZENESW,
                Some(Cursor::SizeNS) => IDC_SIZENS,
                Some(Cursor::SizeWE) => IDC_SIZEWE,
                Some(Cursor::IBeam) => IDC_IBEAM,
                Some(Cursor::Cross) => IDC_CROSS,
                Some(Cursor::Arrow) | None => IDC_ARROW,
            };
            SetCursor(LoadCursorW(None, id).ok());
            LRESULT(1)
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
            with_app(|a| a.remember_views());
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_SYSCHAR if matches!(wparam.0 as u8, b'v' | b'V') => {
            if let Some((x, y)) = with_app(|a| a.view_menu_point()) {
                show_menu(hwnd, VIEW_MENU.get(), x, y);
            }
            LRESULT(0)
        }
        WM_SYSCHAR if matches!(wparam.0 as u8, b'e' | b'E') => {
            if let Some((x, y)) = with_app(|a| a.edit_menu_point()) {
                show_menu(hwnd, EDIT_MENU.get(), x, y);
            }
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
            let checked = match settings::overlay_save() {
                OverlaySave::Ask => ID_SAVE_ASK,
                OverlaySave::Annotate => ID_SAVE_ANNOTATE,
                OverlaySave::Flatten => ID_SAVE_FLATTEN,
            };
            let _ = CheckMenuRadioItem(menu, ID_SAVE_ASK as u32, ID_SAVE_FLATTEN as u32, checked as u32, MF_BYCOMMAND.0);
            let (fit, spread) = with_app(|a| a.view_mode()).flatten().unwrap_or_default();
            let check = |on: bool| MF_BYCOMMAND.0 | if on { MF_CHECKED.0 } else { MF_UNCHECKED.0 };
            CheckMenuItem(menu, ID_FIT_WIDTH as u32, check(fit == tab::Fit::Width));
            CheckMenuItem(menu, ID_FIT_PAGE as u32, check(fit == tab::Fit::Page));
            CheckMenuItem(menu, ID_TWO_PAGES as u32, check(spread));
            let (undo, redo) = with_app(|a| a.undo_state()).unwrap_or_default();
            let _ = EnableMenuItem(menu, ID_UNDO as u32, state(undo));
            let _ = EnableMenuItem(menu, ID_REDO as u32, state(redo));
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
            Some(Action::EditMenu(x, y)) => return show_menu(hwnd, EDIT_MENU.get(), x, y),
            Some(Action::ViewMenu(x, y)) => return show_menu(hwnd, VIEW_MENU.get(), x, y),
            Some(Action::PickOpen(title)) => {
                refresh(hwnd);
                let paths = dialog::pick_pdfs_titled(hwnd, title);
                return perform(hwnd, with_app(|a| a.job_files(paths)));
            }
            Some(Action::PickFolder(title)) => {
                refresh(hwnd);
                let folder = dialog::pick_folder(hwnd, title);
                let cursor = SetCursor(LoadCursorW(None, IDC_WAIT).ok());
                let next = with_app(|a| a.job_folder(folder));
                SetCursor(Some(cursor));
                return perform(hwnd, next);
            }
            Some(Action::PickSave(name)) => {
                refresh(hwnd);
                let path = dialog::save_pdf(hwnd, &name);
                let cursor = SetCursor(LoadCursorW(None, IDC_WAIT).ok());
                let next = with_app(|a| a.job_save(path));
                SetCursor(Some(cursor));
                return perform(hwnd, next);
            }
            Some(Action::OpenWithPassword(path, password)) => {
                match Document::open(&path, Some(&password)) {
                    Ok(doc) => {
                        with_app(|a| a.add_document(doc));
                    }
                    Err(document::OpenError::Password) => {
                        with_app(|a| a.ask_password(path, true));
                    }
                    Err(e) => error_box(hwnd, &e.message(&path)),
                }
            }
            Some(Action::Info(text)) => {
                refresh(hwnd);
                MessageBoxW(Some(hwnd), &HSTRING::from(text), w!("Best PDF"), MB_OK | MB_ICONINFORMATION);
                return;
            }
            Some(Action::ImportSignature) => {
                if let Some(path) = dialog::pick_image(hwnd) {
                    let picture = imageio::load(&path).and_then(|img| {
                        signature::prepare_import(&img).ok_or_else(|| format!("{} looks blank.", path.display()))
                    });
                    match picture {
                        Ok(img) => {
                            with_app(|a| a.pad_set_image(img));
                        }
                        Err(e) => error_box(hwnd, &e),
                    }
                }
            }
            Some(Action::DeleteSignature(i)) => {
                let text = w!("Delete this from your saved signatures?\n\nPlaces where it's already been stamped are not affected.");
                if MessageBoxW(Some(hwnd), text, w!("Best PDF"), MB_OKCANCEL | MB_ICONQUESTION) == IDOK {
                    if let Some(Err(e)) = with_app(|a| a.delete_signature(i)) {
                        error_box(hwnd, &e);
                    }
                }
            }
            Some(Action::Error(e)) => {
                refresh(hwnd);
                return error_box(hwnd, &e);
            }
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
    show_menu(hwnd, FILE_MENU.get(), x, y);
}

fn show_menu(hwnd: HWND, menu: HMENU, x: i32, y: i32) {
    let mut pt = POINT { x, y };
    unsafe {
        let _ = ClientToScreen(hwnd, &mut pt);
        // Modal: runs its own loop and sends WM_COMMAND on selection.
        let _ = TrackPopupMenu(menu, TPM_LEFTALIGN | TPM_TOPALIGN, pt.x, pt.y, None, hwnd, None);
    }
    perform(hwnd, with_app(|a| a.mouse_leave()));
}

/// The right-click menu for page `page`; its commands act on that page.
fn show_page_menu(hwnd: HWND, page: usize, x: i32, y: i32) {
    let mut pt = POINT { x, y };
    let id = unsafe {
        let _ = ClientToScreen(hwnd, &mut pt);
        let flags = TPM_LEFTALIGN | TPM_TOPALIGN | TPM_RETURNCMD | TPM_NONOTIFY;
        TrackPopupMenu(PAGE_MENU.get(), flags, pt.x, pt.y, None, hwnd, None).0 as u16
    };
    let action = match id {
        ID_ROTATE_CW => with_app(|a| a.rotate_page(Some(page), 1)),
        ID_ROTATE_CCW => with_app(|a| a.rotate_page(Some(page), -1)),
        ID_DELETE_PAGE => with_app(|a| a.delete_page(Some(page))),
        ID_EXTRACT => with_app(|a| a.open_extract(Some(page))),
        ID_UNDO | ID_REDO => with_app(|a| a.undo(id == ID_REDO)),
        _ => None,
    };
    perform(hwnd, action);
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
        ID_SAVE_ASK | ID_SAVE_ANNOTATE | ID_SAVE_FLATTEN => {
            let mode = match id {
                ID_SAVE_ANNOTATE => OverlaySave::Annotate,
                ID_SAVE_FLATTEN => OverlaySave::Flatten,
                _ => OverlaySave::Ask,
            };
            if let Err(e) = settings::set_overlay_save(mode) {
                error_box(hwnd, &e);
            }
        }
        ID_UNDO | ID_REDO => perform(hwnd, with_app(|a| a.undo(id == ID_REDO))),
        ID_INSERT_FILE => perform(hwnd, with_app(|a| a.insert_file())),
        ID_COMBINE => perform(hwnd, with_app(|a| a.combine_files())),
        ID_EXTRACT => perform(hwnd, with_app(|a| a.open_extract(None))),
        ID_SPLIT => perform(hwnd, with_app(|a| a.open_split())),
        ID_EXPORT_IMAGES => perform(hwnd, with_app(|a| a.open_export_images())),
        ID_SHRINK => perform(hwnd, with_app(|a| a.open_shrink())),
        ID_GOTO => perform(hwnd, with_app(|a| a.open_goto())),
        ID_FIT_WIDTH => perform(hwnd, with_app(|a| a.set_fit(tab::Fit::Width))),
        ID_FIT_PAGE => perform(hwnd, with_app(|a| a.set_fit(tab::Fit::Page))),
        ID_TWO_PAGES => perform(hwnd, with_app(|a| a.toggle_spread())),
        ID_SELECT_PAGES => perform(hwnd, with_app(|a| a.select_all_pages())),
        ID_FIND => perform(hwnd, with_app(|a| a.open_find())),
        ID_FIND_NEXT => perform(hwnd, with_app(|a| a.find_step(1))),
        ID_FIND_PREV => perform(hwnd, with_app(|a| a.find_step(-1))),
        ID_ROTATE_CW => perform(hwnd, with_app(|a| a.rotate_page(None, 1))),
        ID_ROTATE_CCW => perform(hwnd, with_app(|a| a.rotate_page(None, -1))),
        ID_DELETE_PAGE => perform(hwnd, with_app(|a| a.delete_page(None))),
        ID_ZOOM_IN => perform(hwnd, with_app(|a| a.zoom_step(1))),
        ID_ZOOM_OUT => perform(hwnd, with_app(|a| a.zoom_step(-1))),
        ID_ZOOM_RESET => perform(hwnd, with_app(|a| a.zoom_reset())),
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
    let (mut errors, mut locked) = (Vec::new(), Vec::new());
    for path in paths {
        if with_app(|a| a.focus_path(&path)) == Some(true) {
            continue;
        }
        match Document::open(&path, None) {
            Ok(doc) => {
                with_app(|a| a.add_document(doc));
            }
            Err(document::OpenError::Password) => locked.push(path),
            Err(e) => errors.push(e.message(&path)),
        }
    }
    // Protected files: ask for the first password; the rest wait to be opened again.
    let mut locked = locked.into_iter();
    if let Some(path) = locked.next() {
        with_app(|a| a.ask_password(path, false));
    }
    let others: Vec<String> = locked.map(|p| p.display().to_string()).collect();
    if !others.is_empty() {
        errors.push(format!("These are password-protected too; open them one at a time:\n{}", others.join("\n")));
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
    with_app(|a| a.commit_edits());
    let has_overlays = with_app(|a| a.active_tab().is_some_and(|t| !t.overlays.is_empty())).unwrap_or(false);
    let flatten = match settings::overlay_save() {
        _ if !has_overlays => false,
        OverlaySave::Annotate => false,
        OverlaySave::Flatten => true,
        OverlaySave::Ask => match ask_flatten(hwnd) {
            Some(flatten) => flatten,
            None => return false,
        },
    };
    let result = with_app(|a| {
        a.active_tab_mut().map(|t| {
            let r = t.doc.save_as(path, &t.overlays, flatten);
            if r.is_ok() {
                if flatten {
                    t.flattened();
                } else {
                    t.mark_saved();
                }
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

/// Asks how to save text, highlights and signatures. Some(true) = flatten,
/// None = cancel. "Always do this" updates the setting.
fn ask_flatten(hwnd: HWND) -> Option<bool> {
    const ANNOTATE: i32 = 1001;
    const FLATTEN: i32 = 1002;
    let annotate_text = HSTRING::from("Keep editable\nSaved as annotations: you can still move, resize or delete them later.");
    let flatten_text = HSTRING::from("Flatten into the page\nMade part of the page itself, so they can't be changed or removed afterwards.");
    let buttons = [
        TASKDIALOG_BUTTON { nButtonID: ANNOTATE, pszButtonText: PCWSTR(annotate_text.as_ptr()) },
        TASKDIALOG_BUTTON { nButtonID: FLATTEN, pszButtonText: PCWSTR(flatten_text.as_ptr()) },
    ];
    let config = TASKDIALOGCONFIG {
        cbSize: size_of::<TASKDIALOGCONFIG>() as u32,
        hwndParent: hwnd,
        dwFlags: TDF_USE_COMMAND_LINKS | TDF_ALLOW_DIALOG_CANCELLATION | TDF_POSITION_RELATIVE_TO_WINDOW,
        dwCommonButtons: TDCBF_CANCEL_BUTTON,
        pszWindowTitle: w!("Best PDF"),
        pszMainInstruction: w!("How should text, highlights and signatures be saved?"),
        cButtons: buttons.len() as u32,
        pButtons: buttons.as_ptr(),
        nDefaultButton: ANNOTATE,
        pszVerificationText: w!("Always do this (you can change it in the File menu)"),
        ..Default::default()
    };
    let (mut button, mut always) = (0, BOOL(0));
    unsafe { TaskDialogIndirect(&config, Some(&mut button), None, Some(&mut always)) }.ok()?;
    let flatten = match button {
        ANNOTATE => false,
        FLATTEN => true,
        _ => return None,
    };
    if always.as_bool() {
        let mode = if flatten { OverlaySave::Flatten } else { OverlaySave::Annotate };
        if let Err(e) = settings::set_overlay_save(mode) {
            error_box(hwnd, &e);
        }
    }
    Some(flatten)
}

fn print_document(hwnd: HWND) {
    with_app(|a| a.commit_edits());
    let how = document::Export { flatten: true, plain: true, sid: None };
    let Some((title, pages, bytes, current, selected)) = with_app(|a| a.print_info(&how)).flatten() else { return };
    let result = bytes.and_then(|bytes| {
        // Dialog first (modal, re-enters wnd_proc), then spool with a busy cursor.
        let cursor = unsafe { SetCursor(LoadCursorW(None, IDC_WAIT).ok()) };
        let r = print::print(hwnd, &title, bytes, pages, current, &selected);
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

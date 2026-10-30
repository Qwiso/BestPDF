//! Colors and metrics, modeled on VS Code's "Dark Modern" theme.
//! Metrics are logical pixels (96 DPI); scale with `Metrics::px`.

use windows::Win32::Foundation::COLORREF;

pub const fn rgb(hex: u32) -> COLORREF {
    // 0xRRGGBB → COLORREF's 0x00BBGGRR
    COLORREF((hex >> 16 & 0xFF) | (hex & 0xFF00) | (hex & 0xFF) << 16)
}

pub const CHROME_BG: COLORREF = rgb(0x181818); // title, menu, tab strip, status bar
pub const EDITOR_BG: COLORREF = rgb(0x1F1F1F);
pub const BORDER: COLORREF = rgb(0x2B2B2B);
pub const ACCENT: COLORREF = rgb(0x0078D4);
pub const TEXT: COLORREF = rgb(0xCCCCCC);
pub const TEXT_BRIGHT: COLORREF = rgb(0xFFFFFF);
pub const TEXT_DIM: COLORREF = rgb(0x9D9D9D);
pub const TEXT_FAINT: COLORREF = rgb(0x6E6E6E);
pub const HOVER_BG: COLORREF = rgb(0x2D2E2E);
pub const KEY_BG: COLORREF = rgb(0x2B2B2B);
pub const PDF_ICON: COLORREF = rgb(0xE05252);
pub const PAGE_SHADOW: COLORREF = rgb(0x141414);
pub const WHITE: COLORREF = rgb(0xFFFFFF);
/// Minimap slider / scrollbar thumb: #797979 at these alphas (0–255).
pub const SLIDER: COLORREF = rgb(0x797979);
pub const SLIDER_ALPHA: u8 = 0x33;
pub const SLIDER_ALPHA_HOVER: u8 = 0x59;
pub const SLIDER_ALPHA_ACTIVE: u8 = 0x80;

pub const MENU_H: i32 = 30;
pub const TAB_H: i32 = 35;
pub const STATUS_H: i32 = 22;
pub const MINIMAP_W: i32 = 110;
pub const SCROLLBAR_W: i32 = 14;
pub const PAGE_MARGIN: i32 = 16;
pub const PAGE_GAP: i32 = 12;
pub const MAX_PAGE_W: i32 = 1280;
pub const LINE_STEP: i32 = 48;

/// Icon glyphs from the "Segoe MDL2 Assets" font (ships with Windows 10/11).
pub const ICON_CLOSE: &str = "\u{E8BB}";
pub const ICON_HOME: &str = "\u{E80F}";

#[derive(Clone, Copy)]
pub struct Metrics {
    pub dpi: u32,
}

impl Metrics {
    pub fn px(self, logical: i32) -> i32 {
        logical * self.dpi as i32 / 96
    }
}

# Best PDF

A fast, minimal, portable PDF viewer for Windows. Rust + Win32 + PDFium.

## Layout (portable)

```
BestPDF\
  best-pdf.exe
  pdfium.dll
```

No installer, no registry, no runtime. Copy the folder anywhere and run.

## Build

Requires Rust (MSVC toolchain) and the Visual Studio C++ Build Tools.

```powershell
.\scripts\fetch-pdfium.ps1   # one-time: downloads pinned PDFium into vendor/
cargo run -- path\to\file.pdf
.\scripts\package.ps1        # release build → dist\BestPDF\
```

## Use

- **Layout (VS Code style, Dark Modern colors):** menu strip · tabs · page view · minimap of the whole file · slim scrollbar · status bar (page N of M, page size)
- **Tabs:** every file opens in its own tab and keeps its own scroll position. `Ctrl+Tab` / `Ctrl+Shift+Tab` (or `Ctrl+PgDn/PgUp`) to switch, middle-click or × to close. Opening a file that's already open focuses its tab.
- **Minimap:** click to jump, drag the slider to scrub. Long files scroll the minimap proportionally.
- **Single window:** launching with files while Best PDF is running opens them as tabs in the existing window. Open dialog and drag-drop accept multiple files.
- **File menu** (`Alt+F`): New Tab `Ctrl+T` (welcome screen; the next file you open fills it) · New Document `Ctrl+N` (blank Letter page) · Open `Ctrl+O` · Close `Ctrl+W` · Save `Ctrl+S` · Save As `Ctrl+Shift+S` · Print `Ctrl+P`
- **Open:** also by dragging files in, or passing paths on the command line
- **Scroll:** wheel, scrollbar, arrows, `PgUp`/`PgDn`/`Space`, `Home`/`End`
- **Save** writes a temp file and renames it over the original (PDFium reads the source lazily, so writing in place would corrupt it)
- **Print** is vector output: PDFium draws straight into the printer DC, auto-rotating pages to match paper orientation

## Architecture

| File | Role |
|---|---|
| `src/main.rs` | Window, message loop, commands, single-instance hand-off, dark chrome |
| `src/ui.rs` | Menu strip, tabs, page view, minimap, scrollbar, status bar; input + painting |
| `src/tab.rs` | One open document: layout (fit-width), scroll, page + thumbnail caches |
| `src/theme.rs` | Colors and metrics |
| `src/gdi.rs` | GDI drawing helpers |
| `src/document.rs` | The only module that touches PDFium |
| `src/dialog.rs` | Native Open / Save As dialogs |
| `src/print.rs` | Print dialog + vector printing via raw `FPDF_RenderPage` |

Speed choices so far:
- Page sizes are read without loading pages; only visible pages are rendered.
- Rendered bitmaps are cached; pages far from the viewport are evicted.
- During window-edge drags, cached bitmaps are stretched; crisp re-render on release.
- Double-buffered painting, no background erase (no flicker).
- PDFium loads after the window is shown.
- Release profile: fat LTO, single codegen unit, `panic = abort`, stripped.

The app icon lives in `assets/`: `icon-source.webp` is the artwork, and
`scripts/make-icon.ps1` turns it into `best-pdf.ico` (transparent, 16–256 px).
`build.rs` embeds it into the exe via `assets/app.rc` (needs `rc.exe` from the
Windows SDK, installed with the C++ Build Tools). Re-run the script after
changing the artwork.

PDFium version is pinned in `scripts/fetch-pdfium.ps1` and must match the
`pdfium_XXXX` feature in `Cargo.toml`.

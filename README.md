# Best PDF

A fast, minimal, portable PDF viewer for Windows. Rust + Win32 + PDFium.

## Layout (portable)

```
BestPDF\
  best-pdf.exe
  pdfium.dll
  settings.ini      (created when you change a setting)
  signatures\       (your saved signatures and initials, as PNGs)
```

No installer, no registry, no runtime. Copy the folder anywhere and run.
If the folder isn't writable (e.g. under Program Files), settings and
signatures go to `%APPDATA%\BestPDF` instead.

## Build

Requires Rust (MSVC toolchain) and the Visual Studio C++ Build Tools.

```powershell
.\scripts\fetch-pdfium.ps1   # one-time: downloads pinned PDFium into vendor/
cargo run -- path\to\file.pdf
.\scripts\package.ps1        # release build → dist\BestPDF\
```

## Use

- **Layout (VS Code style, Dark Modern colors):** menu strip · tabs · page view · minimap of the whole file · slim scrollbar · status bar (page N of M, zoom, page size)
- **Tabs:** every file opens in its own tab and keeps its own scroll position. `Ctrl+Tab` / `Ctrl+Shift+Tab` (or `Ctrl+PgDn/PgUp`) to switch, middle-click or × to close. Opening a file that's already open focuses its tab.
- **Minimap (page strip):** click a page to jump to it. The highlighted band marks what's on screen (scroll with the scrollbar). Long files scroll the minimap proportionally.
- **Rearrange pages:** drag a page in the minimap. A blue line shows where it lands.
  - Drop on another tab to append it to that PDF, or rest on the tab for a moment to open it and drop at an exact spot.
  - Drop on the empty tab bar (or a welcome tab) to start a new document with that page.
  - Moves by default; hold `Ctrl` to copy. `Esc` cancels. A document's last page can only be copied out.
  - Edited tabs show ●. Closing them, or the window, asks to save first.
- **Rotate and delete pages:** right-click a page (in the view or the minimap), or use the Edit menu for the page in view. `Ctrl+R` / `Ctrl+Shift+R` rotate clockwise / counterclockwise. Text, marks and signatures on a rotated page move with it and stay upright.
- **Undo / redo:** `Ctrl+Z` / `Ctrl+Y` (or `Ctrl+Shift+Z`). Each tab has its own history of page edits (move, copy, rotate, delete, pages dragged in or out) and changes to text, marks and signatures (a typing session is one step). Saving starts a fresh history.
- **Find:** `Ctrl+F` opens a find bar over the page view. Matches are marked on every page; `Enter` / `F3` next, `Shift+Enter` / `Shift+F3` previous, **Aa** for match case, `Esc` closes.
- **Text, highlights, signatures** (tool buttons next to Edit):
  - **Text:** click a page and type; `Enter` for a new line, `Esc` or click outside when done. Double-click text to edit it. The options strip sets font (Sans / Serif / Mono, the PDF standard fonts, so nothing is embedded), size, bold, italic and color, for the selected text or the next one you add.
  - **Highlight:** drag across a page. The options strip switches between highlight, underline and strikethrough, in six colors. Highlights multiply over the page like a highlighter pen, on screen and in the saved PDF.
  - **Sign ▾:** your saved signatures and initials. **+ New** opens a pad to draw one with the mouse, or import a picture (paper turns transparent and the ink is trimmed). Pick one and it acts as a stamp: every click places a copy. Hover a saved one to delete it.
  - Anything placed can be dragged and resized with any tool: corners scale text (font size) and signatures (keeping proportions), highlights stretch from any side. Arrows nudge (`Shift`: further), `Delete` removes, `Esc` returns to Select.
  - **On save** Best PDF asks whether to keep them **editable** (the default: saved as annotations, movable again when reopened, here or in other editors) or **flatten** them into the page. "Always do this", or File › When Saving Text & Signatures, makes the choice stick. Printing always flattens.
- **Single window:** launching with files while Best PDF is running opens them as tabs in the existing window. Open dialog and drag-drop accept multiple files.
- **File menu** (`Alt+F`): New Tab `Ctrl+T` (welcome screen; the next file you open fills it) · New Document `Ctrl+N` (blank Letter page) · Open `Ctrl+O` · Close `Ctrl+W` · Save `Ctrl+S` · Save As `Ctrl+Shift+S` · Print `Ctrl+P`
- **Edit menu** (`Alt+E`): Undo · Redo · Find · Find Next / Previous · Rotate Page Clockwise / Counterclockwise · Delete Page
- **Open:** also by dragging files in, or passing paths on the command line
- **Scroll:** wheel, scrollbar, arrows, `PgUp`/`PgDn`/`Space`, `Home`/`End`. Sideways (when zoomed in): `Shift`+wheel, tilt wheel / touchpad, `←`/`→`, or the bar along the bottom.
- **Zoom:** the widest page fills 75% of the page view by default; zoom from 10% to 250% of the view's width. `Ctrl`+wheel zooms around the pointer; `Ctrl`+`=` / `Ctrl`+`-` step, `Ctrl`+`0` resets. The status bar has − / + and the current zoom (click it to reset). Each tab keeps its own zoom.
- **Save** writes a temp file and renames it over the original (PDFium reads the source lazily, so writing in place would corrupt it)
- **Print** is vector output: PDFium draws straight into the printer DC, auto-rotating pages to match paper orientation

## Architecture

| File | Role |
|---|---|
| `src/main.rs` | Window, message loop, commands, single-instance hand-off, dark chrome |
| `src/ui.rs` | Menu strip, tabs, page view, minimap, scrollbar, status bar; input + painting |
| `src/tab.rs` | One open document: layout (fit-width), scroll, page + thumbnail caches, overlays |
| `src/overlay.rs` | Text / highlight / signature overlays: model, measuring, on-screen drawing |
| `src/stamp.rs` | Overlays in the PDF: written as tagged Stamp annotations or flattened content, read back on open |
| `src/signature.rs` | Signature library (PNG files), image import via WIC, rasterizing drawn strokes |
| `src/signpad.rs` | The draw-or-import sheet for new signatures |
| `src/settings.rs` | Settings file and the portable data folder |
| `src/theme.rs` | Colors and metrics |
| `src/gdi.rs` | GDI drawing helpers |
| `src/document.rs` | Safe document API (open, render, move/copy/delete pages, save); with `stamp.rs`, the only user of `pdfium.rs` |
| `src/pdfium.rs` | Raw PDFium C functions, resolved from `pdfium.dll` at startup |
| `src/dialog.rs` | Native Open / Save As dialogs |
| `src/print.rs` | Print dialog + vector printing into the printer DC |

Speed choices so far:
- Page sizes are read without loading pages; only visible pages are rendered.
- Rendered bitmaps are cached; pages far from the viewport are evicted.
- During window-edge drags, cached bitmaps are stretched; crisp re-render on release.
- Zoomed-in pages over 8 MP are rendered only around what's on screen (plus a screenful of slack), not whole.
- Double-buffered painting, no background erase (no flicker).
- PDFium loads after the window is shown.
- Release profile: fat LTO, single codegen unit, `panic = abort`, stripped.

The app icon lives in `assets/`: `icon-source.webp` is the artwork, and
`scripts/make-icon.ps1` turns it into `best-pdf.ico` (transparent, 16–256 px).
`build.rs` embeds it into the exe via `assets/app.rc` (needs `rc.exe` from the
Windows SDK, installed with the C++ Build Tools). Re-run the script after
changing the artwork.

PDFium is called directly (no wrapper crate): `src/pdfium.rs` lists the ~60
functions used. The DLL version is pinned in `scripts/fetch-pdfium.ps1`; when
upgrading, check those signatures against the new headers in
`vendor/pdfium/include`.

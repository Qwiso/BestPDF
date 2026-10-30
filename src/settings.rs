//! Settings and the signature library live next to the exe (portable), or in
//! %APPDATA%\BestPDF when that folder isn't writable (e.g. Program Files).

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// What Save does with overlays.
#[derive(Clone, Copy, PartialEq)]
pub enum OverlaySave {
    Ask,
    Annotate,
    Flatten,
}

const FILE: &str = "settings.ini";
const KEY: &str = "overlays-on-save";

pub fn data_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf));
        if let Some(dir) = exe_dir.filter(|d| writable(d)) {
            return dir;
        }
        let dir = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir).join("BestPDF");
        let _ = std::fs::create_dir_all(&dir);
        dir
    })
}

fn writable(dir: &Path) -> bool {
    let probe = dir.join(".bestpdf-probe");
    let ok = std::fs::write(&probe, b"").is_ok();
    let _ = std::fs::remove_file(probe);
    ok
}

pub fn overlay_save() -> OverlaySave {
    let text = std::fs::read_to_string(data_dir().join(FILE)).unwrap_or_default();
    let value = text.lines().find_map(|l| l.trim().strip_prefix(KEY)?.trim_start().strip_prefix('=').map(str::trim));
    match value {
        Some("annotate") => OverlaySave::Annotate,
        Some("flatten") => OverlaySave::Flatten,
        _ => OverlaySave::Ask,
    }
}

pub fn set_overlay_save(mode: OverlaySave) -> Result<(), String> {
    let value = match mode {
        OverlaySave::Ask => "ask",
        OverlaySave::Annotate => "annotate",
        OverlaySave::Flatten => "flatten",
    };
    let path = data_dir().join(FILE);
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let mut lines: Vec<String> = text.lines().filter(|l| !l.trim().starts_with(KEY)).map(String::from).collect();
    lines.push(format!("{KEY} = {value}"));
    std::fs::write(&path, lines.join("\r\n") + "\r\n").map_err(|e| format!("Could not save settings to {}: {e}", path.display()))
}

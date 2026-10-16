//! Where the iPad app keeps its files, without a native document picker yet.
//!
//! The app's `Documents` folder is shared with the Files app ("On My iPad › GridCraft", via
//! `UIFileSharingEnabled` and `LSSupportsOpeningDocumentsInPlace` in `packaging/ios/Info.plist`).
//! Workbooks put there show up under File › Open › Recent; Save As writes there. Plain Rust, so
//! it is tested on every platform.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Extensions GridCraft opens (`gridcraft_engine::io::FileKind`), listed under Recent.
const WORKBOOKS: [&str; 9] = ["xlsx", "xlsm", "xls", "xlsb", "ods", "csv", "tsv", "txt", "json"];

/// Most entries kept in the recent list (`SheetApp::open_path` keeps 20).
const MAX_RECENT: usize = 20;

/// The sandbox's `Documents` folder (`$HOME/Documents` inside an iOS app container).
pub fn documents_dir(home: &Path) -> PathBuf {
    home.join("Documents")
}

/// The settings folder, next to the desktop app's macOS location inside the sandbox.
pub fn config_dir(home: &Path) -> PathBuf {
    home.join("Library/Application Support/GridCraft")
}

/// Workbooks directly in `dir`, newest first. A folder that can't be read lists nothing.
pub fn workbooks_in(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut found: Vec<(SystemTime, String)> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|e| {
            let path = e.path();
            let ext = path.extension()?.to_str()?.to_ascii_lowercase();
            if !WORKBOOKS.contains(&ext.as_str()) {
                return None;
            }
            let modified = e.metadata().and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
            Some((modified, path.to_str()?.to_string()))
        })
        .collect();
    found.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    found.into_iter().map(|(_, p)| p).collect()
}

/// The recent list after a scan of `Documents`: entries whose file is gone are dropped (the
/// sandbox path changes when the app is reinstalled), then the workbooks found that aren't
/// listed yet are added after the ones already there.
pub fn merge_recent(recent: &[String], found: &[String]) -> Vec<String> {
    let mut out: Vec<String> = recent.iter().filter(|p| Path::new(p).is_file()).cloned().collect();
    for p in found {
        if !out.contains(p) {
            out.push(p.clone());
        }
    }
    out.truncate(MAX_RECENT);
    out
}

/// Where Save As writes `suggested` (a title such as `Book1` or `Sales.xlsx`): its file name in
/// `dir`, with `.xlsx` when it has no extension. An existing file is never overwritten: the name
/// gets ` (2)`, ` (3)`… instead.
pub fn save_path(dir: &Path, suggested: &str) -> PathBuf {
    let name = Path::new(suggested).file_name().and_then(|n| n.to_str()).map(str::trim).filter(|n| !n.is_empty() && *n != "." && *n != "..");
    let name = name.unwrap_or("Book1");
    let base = Path::new(name);
    let stem = base.file_stem().and_then(|s| s.to_str()).unwrap_or("Book1").to_string();
    let ext = base.extension().and_then(|e| e.to_str()).unwrap_or("xlsx").to_string();
    let first = dir.join(format!("{stem}.{ext}"));
    if !first.exists() {
        return first;
    }
    (2..10_000u32).map(|n| dir.join(format!("{stem} ({n}).{ext}"))).find(|p| !p.exists()).unwrap_or(first)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("gridcraft-ios-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    #[test]
    fn workbooks_are_listed_and_other_files_are_not() {
        let dir = temp_dir("list");
        for f in ["a.xlsx", "b.CSV", "notes.md", "c.ods"] {
            let _ = std::fs::write(dir.join(f), b"x");
        }
        let _ = std::fs::create_dir_all(dir.join("folder.xlsx"));
        let mut found = workbooks_in(&dir);
        found.sort();
        let names: Vec<String> = found.iter().filter_map(|p| Path::new(p).file_name()?.to_str().map(str::to_string)).collect();
        assert_eq!(names, ["a.xlsx", "b.CSV", "c.ods"]);
        assert!(workbooks_in(&dir.join("missing")).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recent_drops_missing_files_and_adds_new_ones() {
        let dir = temp_dir("recent");
        let (a, b) = (dir.join("a.xlsx"), dir.join("b.xlsx"));
        let _ = std::fs::write(&a, b"x");
        let _ = std::fs::write(&b, b"x");
        let a = a.to_string_lossy().into_owned();
        let b = b.to_string_lossy().into_owned();
        let gone = dir.join("gone.xlsx").to_string_lossy().into_owned();
        let merged = merge_recent(&[gone, a.clone()], &[b.clone(), a.clone()]);
        assert_eq!(merged, [a, b]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_as_never_overwrites() {
        let dir = temp_dir("save");
        assert_eq!(save_path(&dir, "Book1"), dir.join("Book1.xlsx"));
        assert_eq!(save_path(&dir, "/elsewhere/Sales.csv"), dir.join("Sales.csv"));
        assert_eq!(save_path(&dir, ""), dir.join("Book1.xlsx"));
        assert_eq!(save_path(&dir, ".."), dir.join("Book1.xlsx"));
        let _ = std::fs::write(dir.join("Book1.xlsx"), b"x");
        assert_eq!(save_path(&dir, "Book1.xlsx"), dir.join("Book1 (2).xlsx"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

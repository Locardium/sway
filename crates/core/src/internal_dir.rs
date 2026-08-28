//! Where `trash/` and `incoming/` live relative to the managed music
//! folder.
//!
//! On desktop `music_dir` is `<sway>/library`: the internal dirs sit next to
//! it, in `<sway>`, so they never look like tracks to the scanner. On
//! Android there's no `library` split — `music_dir` is already the app's
//! private folder — so the internal dirs live inside it directly.

use std::path::{Path, PathBuf};

pub fn internal_base(music_dir: &Path) -> PathBuf {
    if music_dir.file_name() == Some(std::ffi::OsStr::new("library")) {
        music_dir
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| music_dir.to_path_buf())
    } else {
        music_dir.to_path_buf()
    }
}

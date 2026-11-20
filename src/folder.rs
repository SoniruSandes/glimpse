//! Listing a folder for the folder preview (one level; the UI navigates into sub-folders).

use std::{
    fs,
    path::{Path, PathBuf},
};

const MAX_ENTRIES: usize = 20_000;

pub struct FolderEntry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub size: u64,
}

pub struct FolderListing {
    pub dir: PathBuf,
    pub entries: Vec<FolderEntry>,
    pub truncated: bool,
}

pub fn list(dir: &Path) -> Result<FolderListing, String> {
    let read_dir = fs::read_dir(dir).map_err(|e| e.to_string())?;
    let mut entries = Vec::new();
    let mut truncated = false;
    for item in read_dir.flatten() {
        if entries.len() >= MAX_ENTRIES {
            truncated = true;
            break;
        }
        let Ok(meta) = item.metadata() else { continue };
        entries.push(FolderEntry {
            name: item.file_name().to_string_lossy().into_owned(),
            path: item.path(),
            is_dir: meta.is_dir(),
            size: meta.len(),
        });
    }
    // Folders first, then files, each alphabetical (case-insensitive).
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(FolderListing { dir: dir.to_path_buf(), entries, truncated })
}

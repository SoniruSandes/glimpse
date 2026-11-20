//! Listing archives (zip, 7z, rar) and extracting one entry to a temp file so it can be previewed.

use std::{
    fs::{self, File},
    io::{self, Read},
    path::{Path, PathBuf},
};

pub const MAX_ENTRIES: usize = 50_000;
pub const MAX_EXTRACT: u64 = 256 << 20; // 256 MiB per previewed entry

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Zip,
    SevenZ,
    Rar,
}

#[derive(Clone)]
pub struct Entry {
    pub index: usize, // position inside the archive (used for extraction)
    pub name: String,
    pub size: u64,
    pub is_dir: bool,
}

pub struct Listing {
    pub path: PathBuf,
    pub kind: Kind,
    pub entries: Vec<Entry>,
    pub truncated: bool,
}

pub fn kind_for_ext(ext: &str) -> Option<Kind> {
    match ext {
        "zip" | "jar" | "war" | "apk" | "cbz" => Some(Kind::Zip),
        "7z" => Some(Kind::SevenZ),
        "rar" | "cbr" => Some(Kind::Rar),
        _ => None,
    }
}

pub fn list(path: &Path, kind: Kind) -> Result<Listing, String> {
    let (mut entries, truncated) = match kind {
        Kind::Zip => list_zip(path)?,
        Kind::SevenZ => list_7z(path)?,
        Kind::Rar => list_rar(path)?,
    };
    entries.sort_by_key(|e| e.name.to_lowercase());
    Ok(Listing { path: path.to_path_buf(), kind, entries, truncated })
}

/// Extract one entry into this preview's temp folder and return its path.
pub fn extract(path: &Path, kind: Kind, entry: &Entry) -> Result<PathBuf, String> {
    if entry.size > MAX_EXTRACT {
        return Err(format!("{} is too large to preview", crate::content::human_size(entry.size)));
    }
    let out = out_path(entry)?;
    match kind {
        Kind::Zip => extract_zip(path, entry, &out)?,
        Kind::SevenZ => extract_7z(path, entry, &out)?,
        Kind::Rar => extract_rar(path, entry, &out)?,
    }
    Ok(out)
}

// ---- temp files ----

fn temp_root_for(pid: u32) -> PathBuf {
    std::env::temp_dir().join(format!("quicklook-rs-{pid}"))
}

fn out_path(entry: &Entry) -> Result<PathBuf, String> {
    let dir = temp_root_for(std::process::id());
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let base = entry.name.rsplit(['/', '\\']).next().unwrap_or("file");
    let safe: String = base
        .chars()
        .map(|c| if c.is_alphanumeric() || matches!(c, '.' | '-' | '_' | ' ') { c } else { '_' })
        .collect();
    // The entry's own path is never used, so "../" tricks can't escape the temp folder.
    Ok(dir.join(format!("{}_{}", entry.index, safe)))
}

/// Called by the preview process when it exits normally.
pub fn cleanup_current() {
    let _ = fs::remove_dir_all(temp_root_for(std::process::id()));
}

/// Called by the tray when it kills a preview process.
pub fn cleanup_pid(pid: u32) {
    let _ = fs::remove_dir_all(temp_root_for(pid));
}

/// Called when the tray starts: removes temp folders left by crashed previews.
pub fn cleanup_stale() {
    if let Ok(read_dir) = fs::read_dir(std::env::temp_dir()) {
        for item in read_dir.flatten() {
            if item.file_name().to_string_lossy().starts_with("quicklook-rs-") {
                let path = item.path();
                if path.is_dir() {
                    let _ = fs::remove_dir_all(path);
                } else {
                    let _ = fs::remove_file(path); // pin markers
                }
            }
        }
    }
}

// ---- zip ----

fn list_zip(path: &Path) -> Result<(Vec<Entry>, bool), String> {
    let file = File::open(path).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;
    let total = zip.len();
    let mut entries = Vec::with_capacity(total.min(MAX_ENTRIES));
    for i in 0..total.min(MAX_ENTRIES) {
        // "raw" so that encrypted entries still list.
        let f = zip.by_index_raw(i).map_err(|e| e.to_string())?;
        entries.push(Entry { index: i, name: f.name().to_string(), size: f.size(), is_dir: f.is_dir() });
    }
    Ok((entries, total > MAX_ENTRIES))
}

fn extract_zip(path: &Path, entry: &Entry, out: &Path) -> Result<(), String> {
    let file = File::open(path).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;
    let mut item = zip.by_index(entry.index).map_err(|e| e.to_string())?;
    let mut dest = File::create(out).map_err(|e| e.to_string())?;
    io::copy(&mut (&mut item).take(MAX_EXTRACT), &mut dest).map_err(|e| e.to_string())?;
    Ok(())
}

// ---- 7z ----

fn list_7z(path: &Path) -> Result<(Vec<Entry>, bool), String> {
    let reader = sevenz_rust::SevenZReader::open(path, sevenz_rust::Password::empty()).map_err(|e| e.to_string())?;
    let files = &reader.archive().files;
    let entries = files
        .iter()
        .enumerate()
        .take(MAX_ENTRIES)
        .map(|(i, f)| Entry { index: i, name: f.name().to_string(), size: f.size(), is_dir: f.is_directory() })
        .collect();
    Ok((entries, files.len() > MAX_ENTRIES))
}

fn extract_7z(path: &Path, entry: &Entry, out: &Path) -> Result<(), String> {
    let mut reader =
        sevenz_rust::SevenZReader::open(path, sevenz_rust::Password::empty()).map_err(|e| e.to_string())?;
    let target = entry.index;
    let mut position = 0usize;
    let mut found = false;
    reader
        .for_each_entries(|_item, data| {
            if position == target {
                let mut dest = File::create(out)?;
                io::copy(&mut data.take(MAX_EXTRACT), &mut dest)?;
                found = true;
                return Ok(false); // stop
            }
            // Solid archives must be read through in order, so skip by draining.
            io::copy(data, &mut io::sink())?;
            position += 1;
            Ok(true)
        })
        .map_err(|e| e.to_string())?;
    if found {
        Ok(())
    } else {
        Err("Entry not found in archive".into())
    }
}

// ---- rar (optional feature: needs the unrar C++ library) ----

#[cfg(feature = "rar")]
fn list_rar(path: &Path) -> Result<(Vec<Entry>, bool), String> {
    let archive = unrar::Archive::new(path).open_for_listing().map_err(|e| e.to_string())?;
    let mut entries = Vec::new();
    let mut truncated = false;
    for (i, header) in archive.enumerate() {
        if i >= MAX_ENTRIES {
            truncated = true;
            break;
        }
        let header = header.map_err(|e| e.to_string())?;
        entries.push(Entry {
            index: i,
            name: header.filename.to_string_lossy().into_owned(),
            size: header.unpacked_size,
            is_dir: header.is_directory(),
        });
    }
    Ok((entries, truncated))
}

#[cfg(feature = "rar")]
fn extract_rar(path: &Path, entry: &Entry, out: &Path) -> Result<(), String> {
    let mut archive = unrar::Archive::new(path).open_for_processing().map_err(|e| e.to_string())?;
    let mut position = 0usize;
    while let Some(header) = archive.read_header().map_err(|e| e.to_string())? {
        if position == entry.index {
            let (data, _rest) = header.read().map_err(|e| e.to_string())?;
            fs::write(out, data).map_err(|e| e.to_string())?;
            return Ok(());
        }
        archive = header.skip().map_err(|e| e.to_string())?;
        position += 1;
    }
    Err("Entry not found in archive".into())
}

#[cfg(not(feature = "rar"))]
fn list_rar(_path: &Path) -> Result<(Vec<Entry>, bool), String> {
    Err("RAR support wasn't compiled in (build with the default `rar` feature)".into())
}

#[cfg(not(feature = "rar"))]
fn extract_rar(_path: &Path, _entry: &Entry, _out: &Path) -> Result<(), String> {
    Err("RAR support wasn't compiled in".into())
}

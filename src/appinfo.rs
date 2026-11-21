//! Windows programs (.exe): a small info card (icon, name, location, version, size, CPU type).
//! Read with `pelite` (pure Rust), so it works on every platform; nothing is executed or loaded.

use std::{
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

use eframe::egui;
use pelite::{FileMap, PeFile};

/// The preview was opened through a .lnk shortcut (set by preview.rs).
static VIA_SHORTCUT: AtomicBool = AtomicBool::new(false);

pub fn set_via_shortcut(on: bool) {
    VIA_SHORTCUT.store(on, Ordering::Relaxed);
}

/// Extensions shown as an app card.
pub const APP_EXTS: &[&str] = &["exe"];

/// Window size of the card (inner size, points).
pub const WINDOW_SIZE: [f32; 2] = [650.0, 300.0];

pub struct AppInfo {
    pub file_name: String,
    /// Full path of the program.
    pub location: String,
    pub via_shortcut: bool,
    pub size: u64,
    /// "x64", "x86", "ARM64", ...
    pub arch: Option<&'static str>,
    pub file_version: Option<String>,
    pub product_version: Option<String>,
    pub description: Option<String>,
    pub product_name: Option<String>,
    pub company: Option<String>,
    pub icon: Option<egui::ColorImage>,
}

pub fn load(path: &Path) -> Result<AppInfo, String> {
    let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
    let mut info = AppInfo {
        file_name: path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        location: path.display().to_string(),
        via_shortcut: VIA_SHORTCUT.load(Ordering::Relaxed),
        size: meta.len(),
        arch: None,
        file_version: None,
        product_version: None,
        description: None,
        product_name: None,
        company: None,
        icon: None,
    };

    // Not a Windows executable (or damaged): still show name, place and size.
    let Ok(map) = FileMap::open(path) else { return Ok(info) };
    let Ok(pe) = PeFile::from_bytes(map.as_ref()) else { return Ok(info) };

    info.arch = match pe.file_header().Machine {
        0x8664 => Some("x64"),
        0x014c => Some("x86"),
        0xAA64 => Some("ARM64"),
        0x01c0 | 0x01c4 => Some("ARM"),
        0x0200 => Some("IA-64"),
        _ => None,
    };

    let Ok(resources) = pe.resources() else { return Ok(info) };

    if let Ok(version) = resources.version_info() {
        if let Some(lang) = version.translation().first().copied() {
            let get = |key: &str| version.value(lang, key).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
            info.file_version = get("FileVersion");
            info.product_version = get("ProductVersion");
            info.description = get("FileDescription");
            info.product_name = get("ProductName");
            info.company = get("CompanyName");
        }
        if let Some(fixed) = version.fixed() {
            let text = |v: pelite::image::VS_VERSION| format!("{}.{}.{}.{}", v.Major, v.Minor, v.Patch, v.Build);
            info.file_version.get_or_insert_with(|| text(fixed.dwFileVersion));
            info.product_version.get_or_insert_with(|| text(fixed.dwProductVersion));
        }
    }

    // The first icon group is the program's icon; take its largest picture (up to 256 px).
    if let Some(Ok((_, group))) = resources.icons().next() {
        let best = group
            .entries()
            .iter()
            .filter_map(|e| {
                let side = if e.bWidth == 0 { 256 } else { e.bWidth as u32 };
                group.image(e.nId).ok().map(|data| (side, e.bWidth, e.bHeight, e.wBitCount, data))
            })
            .max_by_key(|(side, _, _, bits, _)| (*side, *bits));
        if let Some((_, w, h, bits, data)) = best {
            info.icon = decode_icon(w, h, bits, data);
        }
    }
    Ok(info)
}

/// One RT_ICON picture: a PNG, or a bare DIB (which we wrap into a one-picture .ico so `image` can decode it).
fn decode_icon(w: u8, h: u8, bits: u16, data: &[u8]) -> Option<egui::ColorImage> {
    let img = if data.starts_with(b"\x89PNG") {
        image::load_from_memory_with_format(data, image::ImageFormat::Png).ok()?
    } else {
        let mut ico = Vec::with_capacity(22 + data.len());
        ico.extend_from_slice(&[0, 0, 1, 0, 1, 0]); // ICONDIR: one picture
        ico.extend_from_slice(&[w, h, 0, 0, 1, 0]);
        ico.extend_from_slice(&bits.to_le_bytes());
        ico.extend_from_slice(&(data.len() as u32).to_le_bytes());
        ico.extend_from_slice(&22u32.to_le_bytes());
        ico.extend_from_slice(data);
        image::load_from_memory_with_format(&ico, image::ImageFormat::Ico).ok()?
    };
    let rgba = img.to_rgba8();
    Some(egui::ColorImage::from_rgba_unmultiplied(
        [rgba.width() as usize, rgba.height() as usize],
        rgba.as_raw(),
    ))
}

#[cfg(test)]
mod tests {
    #[test]
    fn exe_from_env() {
        let Some(path) = std::env::var_os("EXE_SAMPLE") else { return };
        let i = super::load(std::path::Path::new(&path)).unwrap();
        println!(
            "{} {:?} fv={:?} pv={:?} desc={:?} co={:?} icon={:?}",
            i.file_name,
            i.arch,
            i.file_version,
            i.product_version,
            i.description,
            i.company,
            i.icon.as_ref().map(|c| c.size)
        );
    }
}

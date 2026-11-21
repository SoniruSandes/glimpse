//! Windows .lnk shortcuts, parsed in pure Rust (works on every platform): where does the shortcut point?
//!
//! Format: MS-SHLLINK. Header (76 bytes) + optional LinkTargetIDList + optional LinkInfo (holds the local
//! path of the target) + string data (name, relative path, working dir, ...).
//! On Windows, Explorer's own COM resolver (explorer.rs) is used first; this covers everything else
//! (a .lnk opened from the command line, or previewed on Linux).

use std::path::{Path, PathBuf};

fn u16_at(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(o..o + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?))
}

fn cstr_ansi(b: &[u8], o: usize) -> Option<String> {
    let tail = b.get(o..)?;
    let end = tail.iter().position(|&c| c == 0)?;
    Some(String::from_utf8_lossy(&tail[..end]).into_owned())
}

fn cstr_utf16(b: &[u8], o: usize) -> Option<String> {
    let tail = b.get(o..)?;
    let units: Vec<u16> = tail
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|&u| u != 0)
        .collect();
    Some(String::from_utf16_lossy(&units))
}

/// The target path stored in the shortcut (not checked for existence).
pub fn target(lnk: &Path) -> Option<PathBuf> {
    let meta = std::fs::metadata(lnk).ok()?;
    if meta.len() > 1 << 20 {
        return None;
    }
    let b = std::fs::read(lnk).ok()?;
    if u32_at(&b, 0)? != 0x4C {
        return None;
    }
    let flags = u32_at(&b, 0x14)?;
    let (has_idlist, has_info) = (flags & 1 != 0, flags & 2 != 0);
    let unicode = flags & 0x80 != 0;
    let mut pos = 0x4C;
    if has_idlist {
        pos += 2 + u16_at(&b, pos)? as usize;
    }

    // LinkInfo: the local base path (+ suffix).
    if has_info {
        let info = pos;
        let size = u32_at(&b, info)? as usize;
        let header_size = u32_at(&b, info + 4)?;
        let info_flags = u32_at(&b, info + 8)?;
        if info_flags & 1 != 0 {
            let local = if header_size >= 0x24 {
                let (base, suffix) = (u32_at(&b, info + 0x1C)? as usize, u32_at(&b, info + 0x20)? as usize);
                cstr_utf16(&b, info + base).map(|p| (p, cstr_utf16(&b, info + suffix).unwrap_or_default()))
            } else {
                let (base, suffix) = (u32_at(&b, info + 0x10)? as usize, u32_at(&b, info + 0x18)? as usize);
                cstr_ansi(&b, info + base).map(|p| (p, cstr_ansi(&b, info + suffix).unwrap_or_default()))
            };
            if let Some((base, suffix)) = local.filter(|(p, _)| !p.is_empty()) {
                return Some(PathBuf::from(format!("{base}{suffix}")));
            }
        }
        pos = info + size;
    }

    // String data: name, relative path, ... (each: u16 character count + characters).
    let read_string = |pos: &mut usize| -> Option<String> {
        let count = u16_at(&b, *pos)? as usize;
        *pos += 2;
        let text = if unicode {
            let bytes = b.get(*pos..*pos + count * 2)?;
            *pos += count * 2;
            let units: Vec<u16> = bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
            String::from_utf16_lossy(&units)
        } else {
            let bytes = b.get(*pos..*pos + count)?;
            *pos += count;
            String::from_utf8_lossy(bytes).into_owned()
        };
        Some(text)
    };
    if flags & 0x04 != 0 {
        read_string(&mut pos)?; // name / description
    }
    if flags & 0x08 != 0 {
        let relative = read_string(&mut pos)?;
        if !relative.is_empty() {
            let base = lnk.parent().unwrap_or_else(|| Path::new("."));
            return Some(base.join(relative.replace('\\', "/")));
        }
    }
    None
}

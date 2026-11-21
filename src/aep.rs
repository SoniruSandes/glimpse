//! After Effects projects (.aep): a summary of what is inside (AE version, compositions, footage).
//!
//! An .aep is a RIFX container (big-endian RIFF): chunks of `fourcc + u32 length + data`, with `LIST` / `Fold`
//! chunks holding more chunks. The project tree is Fold > Item (one per folder / comp / footage) > Sfdr (a
//! sub-folder's items). The file stores no preview picture, so we show the project's contents instead.
//!
//! The file is *seeked*, not read: only the few small chunks we need are read, so a multi-GB project
//! opens as fast as a tiny one. Layer data (the bulk of the file) is skipped.

use std::{
    fs::File,
    io::{BufReader, Read, Seek, SeekFrom},
    path::Path,
};

const MAX_ITEMS: usize = 20_000;
const MAX_DEPTH: u32 = 24;
const MAX_LEAF: u32 = 64 * 1024;

pub struct Comp {
    pub name: String,
    pub width: u16,
    pub height: u16,
    pub fps: f64,
    /// Seconds.
    pub duration: f64,
}

pub struct Footage {
    pub name: String,
    /// File name of the source on disk (None: solid, placeholder, ...).
    pub file: Option<String>,
}

pub struct AepInfo {
    pub version: Option<AeVersion>,
    pub comps: Vec<Comp>,
    /// Footage items that carry a name (many are stored unnamed).
    pub footage: Vec<Footage>,
    /// All footage items, named or not.
    pub footage_count: usize,
    pub folders: usize,
    /// More items than we list.
    pub truncated: bool,
}

#[derive(Clone, Copy)]
pub struct AeVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl AeVersion {
    /// "After Effects 2025 (25.1.0)"
    pub fn label(&self) -> String {
        let year = match self.major {
            15..=21 => Some(self.major + 2003),
            22..=60 => Some(self.major + 2000),
            _ => None,
        };
        match year {
            Some(y) => format!("After Effects {y} ({}.{}.{})", self.major, self.minor, self.patch),
            None => format!("After Effects {}.{}.{}", self.major, self.minor, self.patch),
        }
    }
}

#[derive(Default)]
struct RawItem {
    kind: u16,
    name: Option<String>,
    cdta: Option<Vec<u8>>,
    alas: Option<Vec<u8>>,
}

struct Walker<R: Read + Seek> {
    r: R,
    version: Option<AeVersion>,
    items: Vec<RawItem>,
    truncated: bool,
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

fn be16(b: &[u8]) -> u16 {
    u16::from_be_bytes([b[0], b[1]])
}

pub fn load(path: &Path) -> Result<AepInfo, String> {
    let file = File::open(path).map_err(|e| e.to_string())?;
    let total = file.metadata().map(|m| m.len()).unwrap_or(0);
    let mut r = BufReader::with_capacity(64 * 1024, file);

    let mut head = [0u8; 12];
    r.read_exact(&mut head).map_err(|_| "Not an After Effects project (file too short)".to_string())?;
    if &head[0..4] != b"RIFX" || &head[8..12] != b"Egg!" {
        // .aep files saved as XML are .aepx; very old / foreign data lands here too.
        return Err("This isn't a binary After Effects project (no RIFX header)".into());
    }
    // The RIFX envelope; anything after it is XMP metadata.
    let end = (8 + be32(&head[4..8]) as u64).min(total);

    let mut w = Walker { r, version: None, items: Vec::new(), truncated: false };
    w.chunks(12, end, 0, true);
    Ok(w.finish())
}

impl<R: Read + Seek> Walker<R> {
    /// Walk the chunks in [pos, end). `top`: the file's root level (looks for `head`).
    fn chunks(&mut self, mut pos: u64, end: u64, depth: u32, top: bool) {
        self.walk(&mut pos, end, depth, top, None);
    }

    fn walk(&mut self, pos: &mut u64, end: u64, depth: u32, top: bool, mut item: Option<&mut RawItem>) {
        while *pos + 8 <= end && self.items.len() < MAX_ITEMS {
            if self.r.seek(SeekFrom::Start(*pos)).is_err() {
                return;
            }
            let mut h = [0u8; 8];
            if self.r.read_exact(&mut h).is_err() {
                return;
            }
            let len = be32(&h[4..8]);
            let data = *pos + 8;
            let data_end = (data + len as u64).min(end);
            let next = data + len as u64 + (len as u64 & 1);
            let fourcc = [h[0], h[1], h[2], h[3]];

            match &fourcc {
                b"LIST" | b"Fold" => {
                    let mut ty = [0u8; 4];
                    if len >= 4 && self.r.read_exact(&mut ty).is_ok() && depth < MAX_DEPTH {
                        match &ty {
                            b"Fold" | b"Sfdr" => {
                                let mut inner = data + 4;
                                self.walk(&mut inner, data_end, depth + 1, false, None);
                            }
                            b"Item" => {
                                let mut raw = RawItem::default();
                                let mut inner = data + 4;
                                self.walk(&mut inner, data_end, depth + 1, false, Some(&mut raw));
                                self.items.push(raw);
                            }
                            _ => {}
                        }
                    }
                    // Other lists (layers, effects, ...) are skipped whole.
                }
                _ if len <= MAX_LEAF => {
                    let mut buf = vec![0u8; len as usize];
                    if self.r.read_exact(&mut buf).is_ok() {
                        self.leaf(&fourcc, buf, top, item.as_deref_mut());
                    }
                }
                _ => {}
            }
            *pos = next;
        }
        if self.items.len() >= MAX_ITEMS {
            self.truncated = true;
        }
    }

    fn leaf(&mut self, fourcc: &[u8; 4], data: Vec<u8>, top: bool, item: Option<&mut RawItem>) {
        match (fourcc, item) {
            (b"head", _) if top && data.len() >= 8 => {
                let bits = be32(&data[4..8]);
                self.version = Some(AeVersion {
                    major: ((bits >> 26) & 0x1f) * 8 + ((bits >> 19) & 0x07),
                    minor: (bits >> 15) & 0x0f,
                    patch: (bits >> 11) & 0x0f,
                });
            }
            (b"idta", Some(item)) if data.len() >= 2 => item.kind = be16(&data),
            (b"Utf8", Some(item)) if item.name.is_none() => {
                item.name = Some(String::from_utf8_lossy(&data).trim_end_matches('\0').to_string());
            }
            (b"cdta", Some(item)) => item.cdta = Some(data),
            (b"alas", Some(item)) => item.alas = Some(data),
            _ => {}
        }
    }

    fn finish(self) -> AepInfo {
        let mut info = AepInfo {
            version: self.version,
            comps: Vec::new(),
            footage: Vec::new(),
            footage_count: 0,
            folders: 0,
            truncated: self.truncated,
        };
        for item in self.items {
            let name = item.name.filter(|n| !n.is_empty());
            match item.kind {
                1 => info.folders += 1,
                4 => {
                    let Some(d) = item.cdta.filter(|d| d.len() >= 160) else { continue };
                    let (dividend, divisor) = (be32(&d[44..48]), be32(&d[48..52]));
                    let frac = be16(&d[158..160]) as f64;
                    info.comps.push(Comp {
                        name: name.unwrap_or_else(|| "Untitled comp".into()),
                        width: be16(&d[140..142]),
                        height: be16(&d[142..144]),
                        fps: be16(&d[156..158]) as f64 + frac / 65536.0,
                        duration: if divisor > 0 { dividend as f64 / divisor as f64 } else { 0.0 },
                    });
                }
                7 => {
                    info.footage_count += 1;
                    let file = item.alas.as_deref().and_then(alas_file_name);
                    if let Some(name) = name {
                        info.footage.push(Footage { name, file });
                    }
                }
                _ => {}
            }
        }
        info
    }
}

/// The `alas` chunk is JSON-ish text with a "fullpath" (or "path"); we want just the file name.
fn alas_file_name(raw: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(raw).replace('\0', "");
    let text = text.trim();
    let path = ["fullpath", "path"].iter().find_map(|key| {
        let pattern = format!("\"{key}\"");
        let after = &text[text.find(&pattern)? + pattern.len()..];
        let after = after[after.find(':')? + 1..].trim_start().strip_prefix('"')?;
        Some(after[..after.find('"')?].to_string())
    });
    let path = path.unwrap_or_else(|| if text.starts_with('{') { String::new() } else { text.to_string() });
    let name = path.rsplit(['/', '\\']).next().unwrap_or("").trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// 90.0 seconds -> "1:30", 3725.5 -> "1:02:05".
pub fn duration_text(seconds: f64) -> String {
    let total = seconds.max(0.0).round() as u64;
    let (h, m, s) = (total / 3600, (total / 60) % 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn sample_from_env() {
        let Some(path) = std::env::var_os("AEP_SAMPLE") else { return };
        let info = super::load(std::path::Path::new(&path)).unwrap();
        println!("{}", info.version.map(|v| v.label()).unwrap_or_default());
        println!("{} comps, {} footage ({} named), {} folders", info.comps.len(), info.footage_count, info.footage.len(), info.folders);
        for c in info.comps.iter().take(4) {
            println!("{} {}x{} {:.2}fps {}", c.name, c.width, c.height, c.fps, super::duration_text(c.duration));
        }
        for f in info.footage.iter().take(4) {
            println!("{} -> {:?}", f.name, f.file);
        }
    }
}

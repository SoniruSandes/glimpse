//! Simplified Office previews: docx (text, headings, tables), pptx (slide text),
//! xlsx/xls/ods (calamine) and csv/tsv. Images and layout are not rendered.

use std::{fs::File, io::Read, path::Path};

use calamine::{open_workbook_auto, Data, Reader};

const MAX_ROWS: usize = 5_000;
const MAX_COLS: usize = 60;
const MAX_BLOCKS: usize = 5_000;
const MAX_SLIDES: usize = 300;
const MAX_XML_BYTES: u64 = 64 << 20;

pub struct Sheet {
    pub name: String,
    pub rows: Vec<Vec<String>>,
    pub ncols: usize,
    pub row0: u32,
    pub col0: u32,
    pub truncated: bool,
}

pub enum Block {
    Heading(u8, String),
    Para(String),
    Bullet(String),
    Table(Vec<Vec<String>>),
}

pub struct Slide {
    pub paras: Vec<String>,
}

// ---------------------------------------------------------------------------------------
// Spreadsheets
// ---------------------------------------------------------------------------------------

pub fn load_workbook(path: &Path) -> Result<Vec<Sheet>, String> {
    let mut workbook = open_workbook_auto(path).map_err(|e| e.to_string())?;
    let mut sheets = Vec::new();
    for name in workbook.sheet_names() {
        let Ok(range) = workbook.worksheet_range(&name) else { continue };
        let (row0, col0) = range.start().unwrap_or((0, 0));
        let mut rows = Vec::new();
        let mut ncols = 0;
        for row in range.rows().take(MAX_ROWS) {
            let cells: Vec<String> = row.iter().take(MAX_COLS).map(cell_text).collect();
            ncols = ncols.max(cells.len());
            rows.push(cells);
        }
        let truncated = range.height() > MAX_ROWS || range.width() > MAX_COLS;
        sheets.push(Sheet { name, rows, ncols, row0, col0, truncated });
    }
    if sheets.is_empty() {
        Err("No readable sheets".into())
    } else {
        Ok(sheets)
    }
}

pub fn load_csv(path: &Path, tab_separated: bool) -> Result<Vec<Sheet>, String> {
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .delimiter(if tab_separated { b'\t' } else { b',' })
        .from_path(path)
        .map_err(|e| e.to_string())?;
    let mut rows = Vec::new();
    let mut ncols = 0;
    let mut truncated = false;
    for record in reader.byte_records() {
        let Ok(record) = record else { continue };
        if rows.len() >= MAX_ROWS {
            truncated = true;
            break;
        }
        let cells: Vec<String> = record
            .iter()
            .take(MAX_COLS)
            .map(|f| String::from_utf8_lossy(f).into_owned())
            .collect();
        ncols = ncols.max(cells.len());
        rows.push(cells);
    }
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    Ok(vec![Sheet { name, rows, ncols, row0: 0, col0: 0, truncated }])
}

fn cell_text(cell: &Data) -> String {
    match cell {
        Data::Empty => String::new(),
        Data::DateTime(dt) => excel_serial_text(dt.as_f64()),
        other => other.to_string(),
    }
}

/// Excel date serial (1900 system) to "YYYY-MM-DD [HH:MM:SS]".
fn excel_serial_text(serial: f64) -> String {
    let days = serial.floor();
    let secs = ((serial - days) * 86_400.0).round() as i64;
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if days < 1.0 {
        return format!("{h:02}:{m:02}:{s:02}");
    }
    let (y, mo, d) = civil_from_days(days as i64 - 25_569); // 25569 = 1970-01-01
    if secs == 0 {
        format!("{y:04}-{mo:02}-{d:02}")
    } else {
        format!("{y:04}-{mo:02}-{d:02} {h:02}:{m:02}:{s:02}")
    }
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

// ---------------------------------------------------------------------------------------
// docx
// ---------------------------------------------------------------------------------------

pub fn load_docx(path: &Path) -> Result<Vec<Block>, String> {
    let xml = read_zip_text(path, "word/document.xml")?;
    let tokens = tokenize(&xml);

    let mut blocks: Vec<Block> = Vec::new();
    let mut para = String::new();
    let mut style: Option<String> = None;
    let mut numbered = false;
    let mut in_text = false;

    let mut table_depth = 0u32;
    let mut table: Vec<Vec<String>> = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut cell = String::new();

    for token in tokens {
        match token {
            Tok::Open { name, attrs, empty } => match name {
                "w:tbl" => {
                    table_depth += 1;
                    if table_depth == 1 {
                        table.clear();
                    }
                }
                "w:tr" if table_depth == 1 => row.clear(),
                "w:tc" if table_depth == 1 => cell.clear(),
                "w:p" => {
                    para.clear();
                    style = None;
                    numbered = false;
                }
                "w:pStyle" => style = attr(attrs, "w:val").map(str::to_string),
                "w:numPr" => numbered = true,
                "w:t" => in_text = !empty,
                "w:tab" => para.push('\t'),
                "w:br" | "w:cr" => para.push('\n'),
                _ => {}
            },
            Tok::Text(text) => {
                if in_text {
                    para.push_str(&unescape(text));
                }
            }
            Tok::Close(name) => match name {
                "w:t" => in_text = false,
                "w:p" => {
                    let text = para.trim_end().to_string();
                    if table_depth > 0 {
                        if table_depth == 1 && !text.is_empty() {
                            if !cell.is_empty() {
                                cell.push('\n');
                            }
                            cell.push_str(&text);
                        }
                    } else if !text.is_empty() {
                        blocks.push(classify(text, style.as_deref(), numbered));
                    }
                }
                "w:tc" if table_depth == 1 => row.push(cell.trim().to_string()),
                "w:tr" if table_depth == 1 => table.push(std::mem::take(&mut row)),
                "w:tbl" => {
                    table_depth = table_depth.saturating_sub(1);
                    if table_depth == 0 && !table.is_empty() {
                        blocks.push(Block::Table(std::mem::take(&mut table)));
                    }
                }
                _ => {}
            },
        }
        if blocks.len() >= MAX_BLOCKS {
            break;
        }
    }
    Ok(blocks)
}

fn classify(text: String, style: Option<&str>, numbered: bool) -> Block {
    if let Some(style) = style {
        if let Some(rest) = style.strip_prefix("Heading") {
            let level = rest.trim().parse::<u8>().unwrap_or(1).clamp(1, 6);
            return Block::Heading(level, text);
        }
        if style == "Title" {
            return Block::Heading(1, text);
        }
        if style == "Subtitle" {
            return Block::Heading(2, text);
        }
    }
    if numbered {
        Block::Bullet(text)
    } else {
        Block::Para(text)
    }
}

// ---------------------------------------------------------------------------------------
// pptx
// ---------------------------------------------------------------------------------------

pub fn load_pptx(path: &Path) -> Result<Vec<Slide>, String> {
    let file = File::open(path).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;

    let mut names: Vec<(u32, String)> = zip
        .file_names()
        .filter_map(|n| {
            let num = n.strip_prefix("ppt/slides/slide")?.strip_suffix(".xml")?.parse::<u32>().ok()?;
            Some((num, n.to_string()))
        })
        .collect();
    names.sort();
    names.truncate(MAX_SLIDES);

    let mut slides = Vec::new();
    for (_, name) in names {
        let mut xml = String::new();
        let mut entry = zip.by_name(&name).map_err(|e| e.to_string())?;
        (&mut entry)
            .take(MAX_XML_BYTES)
            .read_to_string(&mut xml)
            .map_err(|e| e.to_string())?;
        drop(entry);

        let mut paras = Vec::new();
        let mut para = String::new();
        let mut in_text = false;
        for token in tokenize(&xml) {
            match token {
                Tok::Open { name, empty, .. } => match name {
                    "a:p" => para.clear(),
                    "a:t" => in_text = !empty,
                    "a:br" => para.push('\n'),
                    _ => {}
                },
                Tok::Text(text) => {
                    if in_text {
                        para.push_str(&unescape(text));
                    }
                }
                Tok::Close(name) => match name {
                    "a:t" => in_text = false,
                    "a:p" => {
                        let text = para.trim().to_string();
                        if !text.is_empty() {
                            paras.push(text);
                        }
                    }
                    _ => {}
                },
            }
        }
        slides.push(Slide { paras });
    }
    if slides.is_empty() {
        Err("No slides found".into())
    } else {
        Ok(slides)
    }
}

// ---------------------------------------------------------------------------------------
// Tiny XML helpers (text extraction only; no dependency, no DOM)
// ---------------------------------------------------------------------------------------

fn read_zip_text(path: &Path, name: &str) -> Result<String, String> {
    let file = File::open(path).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;
    let mut entry = zip.by_name(name).map_err(|e| format!("{name}: {e}"))?;
    let mut text = String::new();
    (&mut entry)
        .take(MAX_XML_BYTES)
        .read_to_string(&mut text)
        .map_err(|e| e.to_string())?;
    Ok(text)
}

enum Tok<'a> {
    Open { name: &'a str, attrs: &'a str, empty: bool },
    Close(&'a str),
    Text(&'a str),
}

fn tokenize(xml: &str) -> Vec<Tok<'_>> {
    let mut out = Vec::new();
    let mut rest = xml;
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix('<') {
            let Some(end) = after.find('>') else { break };
            let inner = &after[..end];
            rest = &after[end + 1..];
            if inner.starts_with('?') || inner.starts_with('!') {
                continue; // declaration, comment, doctype
            }
            if let Some(name) = inner.strip_prefix('/') {
                out.push(Tok::Close(name.trim()));
                continue;
            }
            let empty = inner.ends_with('/');
            let inner = inner.trim_end_matches('/');
            let (name, attrs) = match inner.find(char::is_whitespace) {
                Some(i) => (&inner[..i], &inner[i..]),
                None => (inner, ""),
            };
            out.push(Tok::Open { name, attrs, empty });
        } else {
            let end = rest.find('<').unwrap_or(rest.len());
            out.push(Tok::Text(&rest[..end]));
            rest = &rest[end..];
        }
    }
    out
}

fn attr<'a>(attrs: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("{key}=\"");
    let start = attrs.find(&needle)? + needle.len();
    let len = attrs[start..].find('"')?;
    Some(&attrs[start..start + len])
}

fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        match rest.find(';') {
            Some(end) if end <= 10 => {
                let entity = &rest[1..end];
                match entity {
                    "lt" => out.push('<'),
                    "gt" => out.push('>'),
                    "amp" => out.push('&'),
                    "quot" => out.push('"'),
                    "apos" => out.push('\''),
                    _ => {
                        let code = if let Some(hex) = entity.strip_prefix("#x").or_else(|| entity.strip_prefix("#X")) {
                            u32::from_str_radix(hex, 16).ok()
                        } else if let Some(dec) = entity.strip_prefix('#') {
                            dec.parse().ok()
                        } else {
                            None
                        };
                        match code.and_then(char::from_u32) {
                            Some(c) => out.push(c),
                            None => out.push_str(&rest[..=end]),
                        }
                    }
                }
                rest = &rest[end + 1..];
            }
            _ => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

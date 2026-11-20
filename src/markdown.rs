//! Markdown files, drawn like GitHub's preview (dark theme), with a switch to the raw source.
//!
//! `parse` runs on the loader thread and turns the text into a small tree (`MdBlock`) and decodes
//! the local images it mentions. `MarkdownView` draws that tree with egui on the UI thread.

use std::{
    cell::RefCell,
    collections::HashMap,
    path::{Path, PathBuf},
};

use eframe::egui::{self, vec2, Align2, Color32, FontId, Margin, Rect, RichText, Sense, Stroke};
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

use crate::{
    content::{text_ui, Chip},
    highlight, icons, theme,
};

// ------------------------------------------------------------------------------------------
// GitHub (dark) look
// ------------------------------------------------------------------------------------------

const BASE: f32 = 15.0;
const MAX_IMAGES: usize = 24;
const MAX_IMAGE_SIDE: u32 = 1600;
const MAX_DEPTH: u32 = 40;

const BG: Color32 = Color32::from_rgb(13, 17, 23);
const TEXT: Color32 = Color32::from_rgb(230, 237, 243);
const STRONG: Color32 = Color32::from_rgb(255, 255, 255);
const MUTED: Color32 = Color32::from_rgb(145, 152, 161);
const LINK: Color32 = Color32::from_rgb(68, 147, 248);
const BORDER: Color32 = Color32::from_rgb(61, 68, 77);
const CODE_BG: Color32 = Color32::from_rgb(21, 27, 35);
const INLINE_CODE_BG: Color32 = Color32::from_rgb(52, 59, 68);

// ------------------------------------------------------------------------------------------
// The parsed document
// ------------------------------------------------------------------------------------------

/// A run of inline text with one style.
#[derive(Clone, Default)]
pub struct Span {
    pub text: String,
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
    pub code: bool,
    /// Alt text of an image that couldn't be shown.
    pub dim: bool,
    pub link: Option<String>,
    /// A line break (the text is empty).
    pub br: bool,
}

impl Span {
    fn same_style(&self, other: &Span) -> bool {
        self.bold == other.bold
            && self.italic == other.italic
            && self.strike == other.strike
            && self.link == other.link
    }
}

#[derive(Clone, Copy)]
pub enum Alert {
    Note,
    Tip,
    Important,
    Warning,
    Caution,
}

impl Alert {
    fn color(self) -> Color32 {
        match self {
            Alert::Note => Color32::from_rgb(68, 147, 248),
            Alert::Tip => Color32::from_rgb(63, 185, 80),
            Alert::Important => Color32::from_rgb(171, 125, 248),
            Alert::Warning => Color32::from_rgb(210, 153, 34),
            Alert::Caution => Color32::from_rgb(248, 81, 73),
        }
    }

    fn title(self) -> &'static str {
        match self {
            Alert::Note => "Note",
            Alert::Tip => "Tip",
            Alert::Important => "Important",
            Alert::Warning => "Warning",
            Alert::Caution => "Caution",
        }
    }
}

const ALERTS: [(&str, Alert); 5] = [
    ("[!NOTE]", Alert::Note),
    ("[!TIP]", Alert::Tip),
    ("[!IMPORTANT]", Alert::Important),
    ("[!WARNING]", Alert::Warning),
    ("[!CAUTION]", Alert::Caution),
];

pub struct Item {
    /// `Some(done)` for a `- [ ]` / `- [x]` task item.
    pub task: Option<bool>,
    pub blocks: Vec<MdBlock>,
}

pub enum MdBlock {
    Heading(u8, Vec<Span>),
    Para(Vec<Span>),
    /// A fenced code block with its language name (may be empty).
    Code { lang: String, text: String },
    /// YAML front matter at the top of the file.
    Meta(String),
    Quote { alert: Option<Alert>, blocks: Vec<MdBlock> },
    List { start: Option<u64>, items: Vec<Item> },
    Table { head: Vec<Vec<Span>>, rows: Vec<Vec<Vec<Span>>> },
    Rule,
    Image { key: String, alt: String, link: Option<String> },
}

pub struct MdDoc {
    pub blocks: Vec<MdBlock>,
    /// Decoded local images, by the path written in the markdown.
    pub images: HashMap<String, egui::ColorImage>,
}

// ------------------------------------------------------------------------------------------
// Parsing (loader thread)
// ------------------------------------------------------------------------------------------

type Events<'a> = std::iter::Peekable<std::vec::IntoIter<Event<'a>>>;

struct Builder<'a> {
    ev: Events<'a>,
    base: PathBuf,
    spans: Vec<Span>,
    italic: u32,
    bold: u32,
    strike: u32,
    links: Vec<String>,
    /// Images found in the current paragraph; they are shown right after it.
    pending: Vec<(String, String, Option<String>)>,
    images: HashMap<String, egui::ColorImage>,
    in_comment: bool,
    depth: u32,
}

pub fn parse(text: &str, path: &Path) -> MdDoc {
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_YAML_STYLE_METADATA_BLOCKS;
    let events: Vec<Event> = Parser::new_ext(text, options).collect();
    let mut builder = Builder {
        ev: events.into_iter().peekable(),
        base: path.parent().map(Path::to_path_buf).unwrap_or_default(),
        spans: Vec::new(),
        italic: 0,
        bold: 0,
        strike: 0,
        links: Vec::new(),
        pending: Vec::new(),
        images: HashMap::new(),
        in_comment: false,
        depth: 0,
    };
    let blocks = builder.blocks();
    MdDoc { blocks, images: builder.images }
}

fn is_inline_start(tag: &Tag) -> bool {
    matches!(tag, Tag::Emphasis | Tag::Strong | Tag::Strikethrough | Tag::Link { .. } | Tag::Image { .. })
}

fn is_inline_end(tag: &TagEnd) -> bool {
    matches!(tag, TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::Link | TagEnd::Image)
}

fn level_num(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

impl<'a> Builder<'a> {
    /// Blocks up to the end of the current container (or the end of the document).
    fn blocks(&mut self) -> Vec<MdBlock> {
        let mut out = Vec::new();
        if self.depth > MAX_DEPTH {
            return out;
        }
        self.depth += 1;
        while let Some(ev) = self.ev.next() {
            match ev {
                Event::Start(Tag::Paragraph) => {}
                Event::End(TagEnd::Paragraph) => self.flush(&mut out),
                Event::Start(Tag::HtmlBlock) => self.flush(&mut out),
                Event::End(TagEnd::HtmlBlock) => self.flush(&mut out),
                Event::Start(Tag::Heading { level, .. }) => {
                    self.flush(&mut out);
                    let spans = self.inline_until_end();
                    out.push(MdBlock::Heading(level_num(level), spans));
                    self.flush(&mut out);
                }
                Event::Start(Tag::BlockQuote(_)) => {
                    self.flush(&mut out);
                    let quote = self.quote();
                    out.push(quote);
                }
                Event::Start(Tag::CodeBlock(kind)) => {
                    self.flush(&mut out);
                    let lang = match kind {
                        CodeBlockKind::Fenced(info) => info.split_whitespace().next().unwrap_or("").to_string(),
                        CodeBlockKind::Indented => String::new(),
                    };
                    let mut code = String::new();
                    while let Some(e) = self.ev.next() {
                        match e {
                            Event::Text(t) => code.push_str(&t),
                            Event::End(_) => break,
                            _ => {}
                        }
                    }
                    if code.ends_with('\n') {
                        code.pop();
                    }
                    out.push(MdBlock::Code { lang, text: code });
                }
                Event::Start(Tag::MetadataBlock(_)) => {
                    self.flush(&mut out);
                    let mut meta = String::new();
                    while let Some(e) = self.ev.next() {
                        match e {
                            Event::Text(t) => meta.push_str(&t),
                            Event::End(_) => break,
                            _ => {}
                        }
                    }
                    out.push(MdBlock::Meta(meta.trim_end().to_string()));
                }
                Event::Start(Tag::List(start)) => {
                    self.flush(&mut out);
                    let list = self.list(start);
                    out.push(list);
                }
                Event::Start(Tag::Table(_)) => {
                    self.flush(&mut out);
                    let table = self.table();
                    out.push(table);
                }
                Event::Rule => {
                    self.flush(&mut out);
                    out.push(MdBlock::Rule);
                }
                Event::Start(t) if is_inline_start(&t) => self.inline(Event::Start(t)),
                Event::End(t) if is_inline_end(&t) => self.inline(Event::End(t)),
                Event::End(_) => {
                    self.flush(&mut out);
                    break;
                }
                Event::Start(_) => {}
                other => self.inline(other),
            }
        }
        self.flush(&mut out);
        self.depth -= 1;
        out
    }

    /// Move the collected inline text (and any images) into `out` as blocks.
    fn flush(&mut self, out: &mut Vec<MdBlock>) {
        if !self.spans.is_empty() {
            let spans = std::mem::take(&mut self.spans);
            let blank = spans.iter().all(|s| !s.code && s.text.trim().is_empty());
            if !blank {
                out.push(MdBlock::Para(spans));
            }
        }
        for (key, alt, link) in self.pending.drain(..) {
            out.push(MdBlock::Image { key, alt, link });
        }
    }

    /// Inline content up to the end tag of the heading / table cell that was just opened.
    fn inline_until_end(&mut self) -> Vec<Span> {
        while let Some(ev) = self.ev.next() {
            match ev {
                Event::End(t) if is_inline_end(&t) => self.inline(Event::End(t)),
                Event::End(_) => break,
                Event::Start(t) if !is_inline_start(&t) => {}
                other => self.inline(other),
            }
        }
        std::mem::take(&mut self.spans)
    }

    fn quote(&mut self) -> MdBlock {
        let mut blocks = self.blocks();
        let mut alert: Option<Alert> = None;
        let mut drop_first = false;
        // GitHub alerts: "> [!NOTE]" on the first line.
        if let Some(MdBlock::Para(spans)) = blocks.first_mut() {
            if let Some(first) = spans.first_mut() {
                if !first.code {
                    let trimmed = first.text.trim_start().to_string();
                    let upper = trimmed.to_ascii_uppercase();
                    for (tag, kind) in ALERTS {
                        if upper.starts_with(tag) {
                            first.text = trimmed[tag.len()..].trim_start().to_string();
                            alert = Some(kind);
                            break;
                        }
                    }
                }
            }
            if alert.is_some() {
                while spans.first().map_or(false, |s| s.br || (!s.code && s.text.is_empty())) {
                    spans.remove(0);
                }
                drop_first = spans.is_empty();
            }
        }
        if drop_first {
            blocks.remove(0);
        }
        MdBlock::Quote { alert, blocks }
    }

    fn list(&mut self, start: Option<u64>) -> MdBlock {
        let mut items = Vec::new();
        while let Some(ev) = self.ev.next() {
            match ev {
                Event::Start(Tag::Item) => {
                    let task = match self.ev.peek() {
                        Some(Event::TaskListMarker(done)) => Some(*done),
                        _ => None,
                    };
                    if task.is_some() {
                        self.ev.next();
                    }
                    let blocks = self.blocks();
                    items.push(Item { task, blocks });
                }
                Event::End(_) => break,
                _ => {}
            }
        }
        MdBlock::List { start, items }
    }

    fn table(&mut self) -> MdBlock {
        let mut head: Vec<Vec<Span>> = Vec::new();
        let mut rows: Vec<Vec<Vec<Span>>> = Vec::new();
        let mut row: Vec<Vec<Span>> = Vec::new();
        while let Some(ev) = self.ev.next() {
            match ev {
                Event::Start(Tag::TableCell) => {
                    let cell = self.inline_until_end();
                    self.pending.clear();
                    row.push(cell);
                }
                Event::End(TagEnd::TableHead) => head = std::mem::take(&mut row),
                Event::End(TagEnd::TableRow) => rows.push(std::mem::take(&mut row)),
                Event::End(_) => break,
                _ => {}
            }
        }
        MdBlock::Table { head, rows }
    }

    // ---- inline ----

    fn style(&self) -> Span {
        Span {
            bold: self.bold > 0,
            italic: self.italic > 0,
            strike: self.strike > 0,
            link: self.links.last().cloned(),
            ..Span::default()
        }
    }

    fn push_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let style = self.style();
        if let Some(last) = self.spans.last_mut() {
            if !last.br && !last.code && !last.dim && last.same_style(&style) {
                last.text.push_str(text);
                return;
            }
        }
        self.spans.push(Span { text: text.to_string(), ..style });
    }

    fn inline(&mut self, ev: Event<'a>) {
        match ev {
            Event::Text(t) => self.push_text(&t),
            Event::Code(t) => {
                let style = self.style();
                self.spans.push(Span { text: t.to_string(), code: true, ..style });
            }
            Event::SoftBreak => self.push_text(" "),
            Event::HardBreak => self.spans.push(Span { br: true, ..Span::default() }),
            Event::Html(h) | Event::InlineHtml(h) => self.html(&h),
            Event::Start(Tag::Emphasis) => self.italic += 1,
            Event::End(TagEnd::Emphasis) => self.italic = self.italic.saturating_sub(1),
            Event::Start(Tag::Strong) => self.bold += 1,
            Event::End(TagEnd::Strong) => self.bold = self.bold.saturating_sub(1),
            Event::Start(Tag::Strikethrough) => self.strike += 1,
            Event::End(TagEnd::Strikethrough) => self.strike = self.strike.saturating_sub(1),
            Event::Start(Tag::Link { dest_url, .. }) => self.links.push(dest_url.to_string()),
            Event::End(TagEnd::Link) => {
                self.links.pop();
            }
            Event::Start(Tag::Image { dest_url, .. }) => {
                let mut alt = String::new();
                while let Some(e) = self.ev.next() {
                    match e {
                        Event::Text(t) | Event::Code(t) => alt.push_str(&t),
                        Event::End(TagEnd::Image) => break,
                        _ => {}
                    }
                }
                self.add_image(&dest_url, alt);
            }
            _ => {}
        }
    }

    /// Raw HTML (README headers, badges, `<img>`): keep the text and images, drop the tags.
    fn html(&mut self, raw: &str) {
        let mut rest = raw;
        if self.in_comment {
            match rest.find("-->") {
                Some(j) => {
                    rest = &rest[j + 3..];
                    self.in_comment = false;
                }
                None => return,
            }
        }
        let mut clean = String::new();
        while let Some(i) = rest.find("<!--") {
            clean.push_str(&rest[..i]);
            match rest[i..].find("-->") {
                Some(j) => rest = &rest[i + j + 3..],
                None => {
                    self.in_comment = true;
                    rest = "";
                }
            }
        }
        clean.push_str(rest);

        let lower = clean.to_ascii_lowercase();
        if lower.contains("<img") {
            if let Some(src) = attr(&clean, "src") {
                let alt = attr(&clean, "alt").unwrap_or_default();
                self.add_image(&src, alt);
            }
        }
        if lower.contains("<br") {
            self.spans.push(Span { br: true, ..Span::default() });
        }
        let text = strip_tags(&clean);
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            self.push_text(trimmed);
            if clean.ends_with('\n') {
                self.push_text(" ");
            }
        }
    }

    /// Local images are decoded now and drawn after the paragraph; anything else shows its alt text.
    fn add_image(&mut self, url: &str, alt: String) {
        let link = self.links.last().cloned();
        if !self.images.contains_key(url) && self.images.len() < MAX_IMAGES {
            if let Some(image) = load_local_image(&self.base, url) {
                self.images.insert(url.to_string(), image);
            }
        }
        if self.images.contains_key(url) {
            self.pending.push((url.to_string(), alt, link));
        } else if !alt.trim().is_empty() {
            let mut span = self.style();
            span.text = alt;
            span.dim = true;
            self.spans.push(span);
        }
    }
}

fn attr(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    for quote in ['"', '\''] {
        let pattern = format!(" {name}={quote}");
        if let Some(i) = lower.find(&pattern) {
            let start = i + pattern.len();
            let end = tag[start..].find(quote)? + start;
            return Some(tag[start..end].to_string());
        }
    }
    None
}

fn strip_tags(raw: &str) -> String {
    let mut out = String::new();
    let mut rest = raw;
    loop {
        match rest.find('<') {
            None => {
                out.push_str(rest);
                break;
            }
            Some(i) => {
                out.push_str(&rest[..i]);
                match rest[i..].find('>') {
                    Some(j) => rest = &rest[i + j + 1..],
                    None => break,
                }
            }
        }
    }
    out.replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

fn load_local_image(base: &Path, url: &str) -> Option<egui::ColorImage> {
    if url.contains("://") || url.starts_with("data:") {
        return None;
    }
    let clean = url.split(|c| c == '#' || c == '?').next()?.replace("%20", " ");
    if clean.is_empty() {
        return None;
    }
    let path = base.join(clean);
    if std::fs::metadata(&path).ok()?.len() > 32 << 20 {
        return None;
    }
    let mut img = image::ImageReader::open(&path).ok()?.with_guessed_format().ok()?.decode().ok()?;
    if img.width() > MAX_IMAGE_SIDE || img.height() > MAX_IMAGE_SIDE {
        img = img.resize(MAX_IMAGE_SIDE, MAX_IMAGE_SIDE, image::imageops::FilterType::Triangle);
    }
    let rgba = img.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    Some(egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw()))
}

// ------------------------------------------------------------------------------------------
// The view (UI thread)
// ------------------------------------------------------------------------------------------

/// Plain text of a span run.
fn spans_text(spans: &[Span]) -> String {
    spans.iter().map(|s| if s.br { "\n" } else { s.text.as_str() }).collect::<String>()
}

/// All the text of a block (for Find).
fn block_text(block: &MdBlock, out: &mut String) {
    match block {
        MdBlock::Heading(_, spans) | MdBlock::Para(spans) => {
            out.push_str(&spans_text(spans));
            out.push('\n');
        }
        MdBlock::Code { text, .. } | MdBlock::Meta(text) => {
            out.push_str(text);
            out.push('\n');
        }
        MdBlock::Quote { blocks, .. } => blocks.iter().for_each(|b| block_text(b, out)),
        MdBlock::List { items, .. } => items.iter().for_each(|item| item.blocks.iter().for_each(|b| block_text(b, out))),
        MdBlock::Table { head, rows } => {
            for cell in head.iter().chain(rows.iter().flatten()) {
                out.push_str(&spans_text(cell));
                out.push(' ');
            }
        }
        MdBlock::Image { alt, .. } => out.push_str(alt),
        MdBlock::Rule => {}
    }
}

/// GitHub-style anchor: "My Heading!" -> "my-heading".
fn slugify(text: &str) -> String {
    text.trim()
        .to_lowercase()
        .chars()
        .filter_map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                Some(c)
            } else if c.is_whitespace() {
                Some('-')
            } else {
                None
            }
        })
        .collect()
}

struct Heading {
    /// Index of the top-level block.
    block: usize,
    level: u8,
    text: String,
    slug: String,
}

pub struct MarkdownView {
    path: PathBuf,
    blocks: Vec<MdBlock>,
    textures: HashMap<String, egui::TextureHandle>,
    lines: Vec<String>,
    truncated: bool,
    /// false = rendered preview, true = raw source.
    source: bool,
    /// Text size (Ctrl/Alt +/-, Ctrl+wheel, pinch).
    scale: f32,
    outline_open: bool,
    headings: Vec<Heading>,
    block_texts: Vec<String>,
    find_open: bool,
    find_text: String,
    find_focus: bool,
    hits: Vec<usize>,
    hit_pos: usize,
    /// Top of each top-level block, from the last frame (scroll-content coordinates).
    block_y: Vec<f32>,
    offset_y: f32,
    scroll_to: Option<f32>,
    /// "#anchor" links clicked in the text.
    goto: RefCell<Option<String>>,
}

#[derive(Clone, Copy)]
struct Env<'a> {
    textures: &'a HashMap<String, egui::TextureHandle>,
    text: Color32,
    scale: f32,
    goto: &'a RefCell<Option<String>>,
}

impl MarkdownView {
    pub fn new(ctx: &egui::Context, path: PathBuf, doc: MdDoc, lines: Vec<String>, truncated: bool) -> Self {
        let mut headings = Vec::new();
        let mut block_texts = Vec::new();
        let mut seen: HashMap<String, usize> = HashMap::new();
        for (i, block) in doc.blocks.iter().enumerate() {
            let mut text = String::new();
            block_text(block, &mut text);
            block_texts.push(text.to_lowercase());
            if let MdBlock::Heading(level, spans) = block {
                let text = spans_text(spans).trim().to_string();
                let base = slugify(&text);
                let count = seen.entry(base.clone()).or_insert(0);
                let slug = if *count == 0 { base.clone() } else { format!("{base}-{count}") };
                *count += 1;
                headings.push(Heading { block: i, level: *level, text, slug });
            }
        }
        let textures = doc
            .images
            .into_iter()
            .enumerate()
            .map(|(i, (key, image))| {
                (key, ctx.load_texture(format!("md-image-{i}"), image, egui::TextureOptions::LINEAR))
            })
            .collect();
        Self {
            path,
            blocks: doc.blocks,
            textures,
            lines,
            truncated,
            source: false,
            scale: 1.0,
            outline_open: false,
            headings,
            block_texts,
            find_open: false,
            find_text: String::new(),
            find_focus: false,
            hits: Vec::new(),
            hit_pos: 0,
            block_y: Vec::new(),
            offset_y: 0.0,
            scroll_to: None,
            goto: RefCell::new(None),
        }
    }

    pub fn chips(&self) -> Vec<Chip> {
        if self.source {
            let mut chips = vec![Chip { icon: icons::CODE, text: format!("{} lines", self.lines.len()) }];
            if self.truncated {
                chips.push(Chip { icon: icons::WARNING, text: "Truncated".into() });
            }
            chips
        } else {
            let mut chips = vec![Chip { icon: icons::FILE_MD, text: "Markdown".into() }];
            if (self.scale - 1.0).abs() > 0.01 {
                chips.push(Chip { icon: icons::MAGNIFYING_GLASS, text: format!("{:.0}%", self.scale * 100.0) });
            }
            chips
        }
    }

    fn scroll_to_block(&mut self, block: usize) {
        if let Some(y) = self.block_y.get(block) {
            self.scroll_to = Some((*y - 14.0).max(0.0));
        }
    }

    fn toggle_find(&mut self) {
        self.find_open = !self.find_open;
        if self.find_open {
            self.find_focus = true;
        } else {
            self.hits.clear();
        }
    }

    fn run_find(&mut self) {
        let query = self.find_text.trim().to_lowercase();
        self.hits = if query.is_empty() {
            Vec::new()
        } else {
            self.block_texts
                .iter()
                .enumerate()
                .filter(|(_, text)| text.contains(&query))
                .map(|(i, _)| i)
                .collect()
        };
        // Start from the first match at or below what is on screen.
        self.hit_pos = self
            .hits
            .iter()
            .position(|&i| self.block_y.get(i).map_or(false, |y| *y + 4.0 >= self.offset_y))
            .unwrap_or(0);
        self.jump_to_hit();
    }

    fn step_hit(&mut self, forward: bool) {
        let n = self.hits.len();
        if n == 0 {
            return;
        }
        self.hit_pos = if forward { (self.hit_pos + 1) % n } else { (self.hit_pos + n - 1) % n };
        self.jump_to_hit();
    }

    fn jump_to_hit(&mut self) {
        if let Some(&block) = self.hits.get(self.hit_pos) {
            self.scroll_to_block(block);
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let name = self.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        ui.horizontal(|ui| {
            if theme::icon_button(ui, icons::EYE, "Preview", !self.source, true).clicked() {
                self.source = false;
            }
            if theme::icon_button(ui, icons::CODE, "Source", self.source, true).clicked() {
                self.source = true;
            }
            if !self.source {
                ui.add_space(2.0);
                let has_outline = !self.headings.is_empty();
                if theme::icon_button(ui, icons::LIST_BULLETS, "Outline", self.outline_open, has_outline).clicked() {
                    self.outline_open = !self.outline_open;
                }
                if theme::icon_button(ui, icons::MAGNIFYING_GLASS, "Find (Ctrl+F)", self.find_open, true).clicked() {
                    self.toggle_find();
                }
            }
            ui.add_space(6.0);
            ui.label(RichText::new(name).size(15.0));
            ui.label(RichText::new("Markdown").weak());
        });
        ui.add_space(2.0);

        if self.source {
            text_ui(ui, &self.lines);
            return;
        }

        // Ctrl+F, text size (Ctrl/Alt +/-, Alt+0 resets, Ctrl+wheel / pinch).
        let area_hovered = ui.rect_contains_pointer(ui.available_rect_before_wrap());
        let (find_key, zoom, reset) = ctx.input(|i| {
            let modifier = i.modifiers.ctrl || i.modifiers.alt;
            let mut factor = if area_hovered { i.zoom_delta() } else { 1.0 };
            if modifier && (i.key_pressed(egui::Key::Plus) || i.key_pressed(egui::Key::Equals)) {
                factor *= 1.1;
            }
            if modifier && i.key_pressed(egui::Key::Minus) {
                factor /= 1.1;
            }
            (
                i.modifiers.command && i.key_pressed(egui::Key::F),
                factor,
                modifier && i.key_pressed(egui::Key::Num0),
            )
        });
        if find_key {
            if self.find_open {
                self.find_focus = true;
            } else {
                self.toggle_find();
            }
        }
        self.scale = if reset { 1.0 } else { (self.scale * zoom).clamp(0.6, 2.2) };

        if self.find_open {
            ui.horizontal(|ui| {
                let edit = ui.add(
                    egui::TextEdit::singleline(&mut self.find_text)
                        .hint_text("Find in document")
                        .desired_width(240.0),
                );
                if self.find_focus {
                    edit.request_focus();
                    self.find_focus = false;
                }
                if edit.changed() {
                    self.run_find();
                }
                if edit.lost_focus() {
                    let (enter, escape, shift) = ctx.input(|i| {
                        (i.key_pressed(egui::Key::Enter), i.key_pressed(egui::Key::Escape), i.modifiers.shift)
                    });
                    if enter {
                        self.step_hit(!shift);
                        edit.request_focus();
                    }
                    if escape {
                        self.toggle_find();
                    }
                }
                let count = if self.hits.is_empty() {
                    if self.find_text.trim().is_empty() {
                        String::new()
                    } else {
                        "No matches".to_string()
                    }
                } else {
                    format!("{} / {}", self.hit_pos + 1, self.hits.len())
                };
                ui.label(RichText::new(count).weak());
                if theme::icon_button(ui, icons::CARET_UP, "Previous (Shift+Enter)", false, !self.hits.is_empty()).clicked() {
                    self.step_hit(false);
                }
                if theme::icon_button(ui, icons::CARET_DOWN, "Next (Enter)", false, !self.hits.is_empty()).clicked() {
                    self.step_hit(true);
                }
                if theme::icon_button(ui, icons::X, "Close find", false, true).clicked() {
                    self.toggle_find();
                }
            });
            ui.add_space(2.0);
        }

        // Outline: the headings, click to jump; the section on screen is highlighted.
        let mut outline_click: Option<usize> = None;
        if self.outline_open && !self.headings.is_empty() {
            let current = self
                .headings
                .iter()
                .rposition(|h| self.block_y.get(h.block).map_or(false, |y| *y <= self.offset_y + 60.0));
            egui::SidePanel::left(ui.id().with("md_outline"))
                .frame(egui::Frame::none().fill(theme::PANEL_FILL).inner_margin(Margin::same(6.0)))
                .resizable(true)
                .default_width(230.0)
                .width_range(150.0..=420.0)
                .show_inside(ui, |ui| {
                    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                        for (n, heading) in self.headings.iter().enumerate() {
                            ui.horizontal(|ui| {
                                ui.add_space(heading.level.saturating_sub(1).min(4) as f32 * 14.0);
                                let color = if current == Some(n) { Color32::WHITE } else { MUTED };
                                let text = RichText::new(heading.text.as_str()).size(13.5).color(color);
                                let response = ui.add(
                                    egui::Label::new(text).wrap_mode(egui::TextWrapMode::Truncate).sense(Sense::click()),
                                );
                                if response.hovered() {
                                    ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
                                }
                                if response.clicked() {
                                    outline_click = Some(heading.block);
                                }
                            });
                        }
                    });
                });
        }
        if let Some(block) = outline_click {
            self.scroll_to_block(block);
        }

        // The document itself.
        let scroll_to = self.scroll_to.take();
        let hits = self.hits.clone();
        let current_hit = self.hits.get(self.hit_pos).copied();
        let mut new_block_y: Vec<f32> = Vec::new();
        let mut new_offset = self.offset_y;
        egui::CentralPanel::default().frame(egui::Frame::none()).show_inside(ui, |ui| {
            let rect = ui.available_rect_before_wrap();
            ui.painter().rect_filled(rect, 8.0, BG);
            let env = Env { textures: &self.textures, text: TEXT, scale: self.scale, goto: &self.goto };
            let blocks = &self.blocks;
            let mut area = egui::ScrollArea::vertical().auto_shrink([false, false]);
            if let Some(y) = scroll_to {
                area = area.vertical_scroll_offset(y);
            }
            let output = area.show(ui, |ui| {
                let origin = ui.min_rect().top();
                let mut ys: Vec<f32> = Vec::with_capacity(blocks.len());
                // A centred column, like GitHub's readme box.
                let side = ((ui.available_width() - 920.0) / 2.0).max(0.0);
                egui::Frame::none()
                    .outer_margin(Margin::symmetric(side, 0.0))
                    .inner_margin(Margin::symmetric(30.0, 26.0))
                    .show(ui, |ui| {
                        for (i, block) in blocks.iter().enumerate() {
                            let top = ui.cursor().top();
                            ys.push(top - origin);
                            ui.push_id(i, |ui| block_ui(ui, block, env));
                            let bottom = ui.cursor().top();
                            if hits.contains(&i) {
                                let alpha = if current_hit == Some(i) { 90 } else { 40 };
                                let band = Rect::from_min_max(
                                    egui::pos2(ui.max_rect().left() - 8.0, top - 3.0),
                                    egui::pos2(ui.max_rect().right() + 8.0, bottom + 3.0),
                                );
                                ui.painter().rect_filled(band, 4.0, Color32::from_rgba_unmultiplied(255, 200, 0, alpha));
                            }
                            if i + 1 < blocks.len() {
                                ui.add_space(14.0);
                            }
                        }
                    });
                ys
            });
            new_block_y = output.inner;
            new_offset = output.state.offset.y;
        });
        self.block_y = new_block_y;
        self.offset_y = new_offset;

        // "#anchor" links in the text jump to the matching heading.
        let requested = self.goto.borrow_mut().take();
        if let Some(anchor) = requested {
            let key = anchor.to_lowercase();
            if let Some(block) = self.headings.iter().find(|h| h.slug == key).map(|h| h.block) {
                self.scroll_to_block(block);
            }
        }
        if self.scroll_to.is_some() {
            ctx.request_repaint();
        }
    }
}

fn open_link(url: &str, env: Env) {
    if let Some(anchor) = url.strip_prefix('#') {
        *env.goto.borrow_mut() = Some(anchor.to_string());
        return;
    }
    let lower = url.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("mailto:") {
        let _ = open::that(url);
    }
}

fn blocks_ui(ui: &mut egui::Ui, blocks: &[MdBlock], env: Env, tight: bool) {
    let gap = if tight { 4.0 } else { 14.0 };
    for (i, block) in blocks.iter().enumerate() {
        ui.push_id(i, |ui| block_ui(ui, block, env));
        if i + 1 < blocks.len() {
            ui.add_space(gap);
        }
    }
}

fn rule_ui(ui: &mut egui::Ui) {
    let (_, rect) = ui.allocate_space(vec2(ui.available_width(), 1.0));
    ui.painter().rect_filled(rect, 0.0, BORDER);
}

fn block_ui(ui: &mut egui::Ui, block: &MdBlock, env: Env) {
    match block {
        MdBlock::Heading(level, spans) => {
            let (size, underline) = match *level {
                1 => (32.0, true),
                2 => (24.0, true),
                3 => (20.0, false),
                4 => (16.0, false),
                5 => (14.0, false),
                _ => (13.5, false),
            };
            let size = size * env.scale;
            let color = if *level == 6 { MUTED } else { STRONG };
            ui.add_space(if *level <= 2 { 10.0 } else { 6.0 });
            inline_ui(ui, spans, size, color, env);
            if underline {
                ui.add_space(6.0);
                rule_ui(ui);
            }
        }
        MdBlock::Para(spans) => inline_ui(ui, spans, BASE * env.scale, env.text, env),
        MdBlock::Code { lang, text } => {
            egui::Frame::none()
                .fill(CODE_BG)
                .rounding(egui::Rounding::same(6.0))
                .inner_margin(Margin::same(14.0))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    // Language name on the left, copy button on the right.
                    ui.horizontal(|ui| {
                        if !lang.is_empty() {
                            ui.label(RichText::new(lang.as_str()).size(12.0 * env.scale).color(MUTED));
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let id = ui.id().with("copied_at");
                            let now = ui.ctx().input(|i| i.time);
                            let copied = ui.data(|d| d.get_temp::<f64>(id)).map_or(false, |t| now - t < 1.5);
                            let (glyph, tip) = if copied { (icons::CHECK, "Copied") } else { (icons::COPY_SIMPLE, "Copy") };
                            if theme::icon_button(ui, glyph, tip, false, true).clicked() {
                                ui.ctx().output_mut(|o| o.copied_text = text.clone());
                                ui.data_mut(|d| d.insert_temp(id, now));
                                ui.ctx().request_repaint_after(std::time::Duration::from_millis(1600));
                            }
                        });
                    });
                    egui::ScrollArea::horizontal().auto_shrink([false, true]).show(ui, |ui| {
                        let job = highlight::layout(lang, text, 13.0 * env.scale, TEXT);
                        ui.add(egui::Label::new(job).wrap_mode(egui::TextWrapMode::Extend));
                    });
                });
        }
        MdBlock::Meta(text) => {
            egui::Frame::none()
                .fill(CODE_BG)
                .stroke(Stroke::new(1.0, BORDER))
                .rounding(egui::Rounding::same(6.0))
                .inner_margin(Margin::same(12.0))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(RichText::new("Front matter").size(12.0 * env.scale).color(MUTED));
                    ui.add(
                        egui::Label::new(RichText::new(text.as_str()).monospace().size(13.0 * env.scale).color(MUTED))
                            .wrap_mode(egui::TextWrapMode::Wrap),
                    );
                });
        }
        MdBlock::Quote { alert, blocks } => {
            let bar = alert.map(|a| a.color()).unwrap_or(BORDER);
            let sub = if alert.is_some() { env } else { Env { text: MUTED, ..env } };
            let response = egui::Frame::none()
                .inner_margin(Margin { left: 16.0, right: 0.0, top: 0.0, bottom: 0.0 })
                .show(ui, |ui| {
                    if let Some(a) = alert {
                        ui.label(RichText::new(a.title()).size(BASE * env.scale).strong().color(a.color()));
                        ui.add_space(4.0);
                    }
                    blocks_ui(ui, blocks, sub, false);
                })
                .response;
            let r = response.rect;
            ui.painter().rect_filled(Rect::from_min_size(r.left_top(), vec2(3.5, r.height())), 2.0, bar);
        }
        MdBlock::List { start, items } => {
            for (n, item) in items.iter().enumerate() {
                ui.push_id(n, |ui| {
                    ui.horizontal_top(|ui| {
                        ui.spacing_mut().item_spacing.x = 2.0;
                        marker_ui(ui, start.map(|s| s + n as u64), item.task, env);
                        ui.vertical(|ui| blocks_ui(ui, &item.blocks, env, true));
                    });
                });
                if n + 1 < items.len() {
                    ui.add_space(4.0);
                }
            }
        }
        MdBlock::Table { head, rows } => {
            let cols = head.len().max(rows.iter().map(|r| r.len()).max().unwrap_or(0)).max(1);
            let col_w = (ui.available_width() / cols as f32).max(120.0);
            egui::ScrollArea::horizontal().auto_shrink([false, true]).show(ui, |ui| {
                egui::Grid::new("md_table").spacing([0.0, 0.0]).show(ui, |ui| {
                    if !head.is_empty() {
                        for c in 0..cols {
                            cell_ui(ui, head.get(c), true, false, col_w, env);
                        }
                        ui.end_row();
                    }
                    for (r, row) in rows.iter().enumerate() {
                        for c in 0..cols {
                            cell_ui(ui, row.get(c), false, r % 2 == 0, col_w, env);
                        }
                        ui.end_row();
                    }
                });
            });
        }
        MdBlock::Rule => {
            ui.add_space(4.0);
            rule_ui(ui);
            ui.add_space(4.0);
        }
        MdBlock::Image { key, alt, link } => match env.textures.get(key) {
            Some(tex) => {
                let size = tex.size_vec2();
                let scale = (ui.available_width() / size.x.max(1.0)).min(1.0);
                let response = ui.image((tex.id(), size * scale)).interact(Sense::click());
                if let Some(url) = link {
                    if response.hovered() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    }
                    if response.clicked() {
                        open_link(url, env);
                    }
                }
            }
            None => {
                ui.label(RichText::new(alt.as_str()).weak());
            }
        },
    }
}

/// Bullet, number, or task checkbox in a fixed-width column.
fn marker_ui(ui: &mut egui::Ui, number: Option<u64>, task: Option<bool>, env: Env) {
    let (rect, _) = ui.allocate_exact_size(vec2(30.0, BASE * 1.45 * env.scale), Sense::hover());
    if let Some(done) = task {
        let boxed = Rect::from_center_size(rect.center() + vec2(2.0, 0.0), vec2(15.0, 15.0));
        let painter = ui.painter();
        if done {
            painter.rect_filled(boxed, 3.0, theme::ACCENT);
            let c = boxed.center();
            let stroke = Stroke::new(2.0, Color32::WHITE);
            painter.line_segment([c + vec2(-4.0, 0.5), c + vec2(-1.0, 3.5)], stroke);
            painter.line_segment([c + vec2(-1.0, 3.5), c + vec2(4.5, -3.5)], stroke);
        } else {
            painter.rect_stroke(boxed, 3.0, Stroke::new(1.5, BORDER));
        }
        return;
    }
    let text = match number {
        Some(n) => format!("{n}."),
        None => "•".to_string(),
    };
    ui.painter().text(
        rect.right_center() - vec2(6.0, 0.0),
        Align2::RIGHT_CENTER,
        text,
        FontId::proportional(BASE * env.scale),
        env.text,
    );
}

fn cell_ui(ui: &mut egui::Ui, spans: Option<&Vec<Span>>, header: bool, shade: bool, col_w: f32, env: Env) {
    let fill = if shade { CODE_BG } else { Color32::TRANSPARENT };
    egui::Frame::none()
        .fill(fill)
        .stroke(Stroke::new(1.0, BORDER))
        .inner_margin(Margin::symmetric(12.0, 6.0))
        .show(ui, |ui| {
            ui.set_max_width((col_w - 25.0).max(60.0));
            match spans {
                Some(spans) => inline_ui(ui, spans, BASE * env.scale, if header { STRONG } else { env.text }, env),
                None => {
                    ui.label("");
                }
            }
        });
}

/// One paragraph of mixed text: wraps across runs; links are clickable.
fn inline_ui(ui: &mut egui::Ui, spans: &[Span], size: f32, color: Color32, env: Env) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = vec2(0.0, 4.0);
        ui.set_row_height(size * 1.45);
        for span in spans {
            if span.br {
                ui.end_row();
                continue;
            }
            let mut text = RichText::new(span.text.as_str());
            text = if span.code {
                text.monospace().size(size * 0.9).color(TEXT).background_color(INLINE_CODE_BG)
            } else {
                let c = if span.dim {
                    MUTED
                } else if span.bold {
                    STRONG
                } else {
                    color
                };
                text.size(size).color(c)
            };
            if span.italic {
                text = text.italics();
            }
            if span.strike {
                text = text.strikethrough();
            }
            match &span.link {
                Some(url) => {
                    let response = ui.add(egui::Label::new(text.color(LINK)).sense(Sense::click()));
                    if response.hovered() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    }
                    if response.clicked() {
                        open_link(url, env);
                    }
                    response.on_hover_text(url.as_str());
                }
                None => {
                    ui.label(text);
                }
            }
        }
    });
}

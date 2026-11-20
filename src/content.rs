//! What the preview shows, and how each file type is loaded (background thread) and drawn (UI thread).

use std::{
    collections::{HashMap, HashSet},
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, Sender},
};

use eframe::egui::{self, pos2, vec2, Align2, Color32, FontId, Rect, RichText, Sense, TextStyle};
use egui_extras::{Column, TableBuilder};

use crate::{
    archive::{self, Entry, Listing},
    folder::{self, FolderListing},
    htmlview::HtmlView,
    icons,
    markdown::{self, MarkdownView},
    media::{self, MediaView, WindowHint},
    model::{self, ModelView},
    passwords,
    theme,
    office::{self, Block, Sheet, Slide},
    pdf::{self, PdfError, PdfLoaded, PdfMsg},
};

const MAX_TEXT_BYTES: u64 = 1 << 20; // 1 MiB
const MAX_TEXT_LINES: usize = 20_000;
const MAX_IMAGE_SIDE: u32 = 4096;
const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp", "ico", "tif", "tiff"];

pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

// ------------------------------------------------------------------------------------------
// Loading (runs off the UI thread)
// ------------------------------------------------------------------------------------------

pub enum Loaded {
    Image(egui::ColorImage),
    Text { lines: Vec<String>, truncated: bool },
    Html { path: PathBuf, lines: Vec<String>, truncated: bool },
    Markdown { path: PathBuf, doc: markdown::MdDoc, lines: Vec<String>, truncated: bool },
    Pdf(PdfLoaded),
    PdfLocked { path: PathBuf },
    Folder(FolderListing),
    Table(Vec<Sheet>),
    Doc(Vec<Block>),
    Slides(Vec<Slide>),
    Archive(Listing),
    Media { path: PathBuf, video: bool },
    Model(Box<model::ModelData>),
    /// An image together with the other images in its folder (Left/Right to browse).
    Gallery { first: Box<Loaded>, files: Vec<PathBuf>, index: usize },
    Unsupported(String),
    Failed(String),
}

fn failed<T>(r: Result<T, String>, ok: impl FnOnce(T) -> Loaded) -> Loaded {
    match r {
        Ok(v) => ok(v),
        Err(e) => Loaded::Failed(e),
    }
}

pub fn load(path: &Path, ctx: &egui::Context) -> Loaded {
    if path.is_dir() {
        return failed(folder::list(path), Loaded::Folder);
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    let ext = ext.as_str();

    if IMAGE_EXTS.contains(&ext) {
        return load_image(path);
    }
    if media::VIDEO_EXTS.contains(&ext) {
        return Loaded::Media { path: path.to_path_buf(), video: true };
    }
    if media::AUDIO_EXTS.contains(&ext) {
        return Loaded::Media { path: path.to_path_buf(), video: false };
    }
    if model::MODEL_EXTS.contains(&ext) {
        return load_model(path, ext);
    }
    match ext {
        "pdf" => load_pdf(path, ctx, None),
        "ai" => {
            // Illustrator files saved with "Create PDF Compatible File" (the default) are PDFs inside.
            if has_pdf_header(path) {
                load_pdf(path, ctx, None)
            } else {
                Loaded::Unsupported(
                    "This Illustrator file has no PDF-compatible preview (it was saved without \"Create PDF Compatible File\").".into(),
                )
            }
        }
        "psd" => load_psd(path),
        "psb" => Loaded::Unsupported("Photoshop Large Document (.psb) isn't supported yet.".into()),
        "docx" | "docm" => failed(office::load_docx(path), Loaded::Doc),
        "pptx" | "pptm" => failed(office::load_pptx(path), Loaded::Slides),
        "xlsx" | "xlsm" | "xlsb" | "xls" | "ods" => failed(office::load_workbook(path), Loaded::Table),
        "csv" | "tsv" => failed(office::load_csv(path, ext == "tsv"), Loaded::Table),
        "svg" | "svgz" => load_svg(path),
        "html" | "htm" | "xhtml" => match load_text(path) {
            Loaded::Text { lines, truncated } => Loaded::Html { path: path.to_path_buf(), lines, truncated },
            other => other,
        },
        "md" | "markdown" | "mdown" | "mkd" => match load_text(path) {
            Loaded::Text { lines, truncated } => {
                let doc = markdown::parse(&lines.join("\n"), path);
                Loaded::Markdown { path: path.to_path_buf(), doc, lines, truncated }
            }
            other => other,
        },
        "doc" | "ppt" => Loaded::Unsupported(
            "Legacy .doc / .ppt files aren't supported yet (.docx, .pptx and .xlsx are).".into(),
        ),
        _ => {
            if let Some(kind) = archive::kind_for_ext(ext) {
                return failed(archive::list(path, kind), Loaded::Archive);
            }
            load_text(path)
        }
    }
}

/// 3D models. If an .obj/.stl/.ply turns out not to be a model (e.g. a compiled Windows .obj), fall back to text.
fn load_model(path: &Path, ext: &str) -> Loaded {
    match model::load(path) {
        Ok(data) => Loaded::Model(Box::new(data)),
        Err(e) if matches!(ext, "obj" | "stl" | "ply") => match load_text(path) {
            Loaded::Unsupported(_) | Loaded::Failed(_) => Loaded::Failed(e),
            text => text,
        },
        Err(e) => Loaded::Failed(e),
    }
}

const GALLERY_EXTS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp", "ico", "tif", "tiff", "svg", "svgz", "psd"];

fn is_gallery_image(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| GALLERY_EXTS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// "img2" before "img10".
fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering::{Equal, Greater, Less};
    let (a, b) = (a.to_lowercase(), b.to_lowercase());
    let (mut ai, mut bi) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (ai.peek().copied(), bi.peek().copied()) {
            (None, None) => return Equal,
            (None, _) => return Less,
            (_, None) => return Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let (mut na, mut nb) = (String::new(), String::new());
                while let Some(c) = ai.peek().copied().filter(|c| c.is_ascii_digit()) {
                    na.push(c);
                    ai.next();
                }
                while let Some(c) = bi.peek().copied().filter(|c| c.is_ascii_digit()) {
                    nb.push(c);
                    bi.next();
                }
                let (na, nb) = (na.trim_start_matches('0'), nb.trim_start_matches('0'));
                let order = na.len().cmp(&nb.len()).then_with(|| na.cmp(nb));
                if order != Equal {
                    return order;
                }
            }
            (Some(x), Some(y)) => {
                if x != y {
                    return x.cmp(&y);
                }
                ai.next();
                bi.next();
            }
        }
    }
}

/// Like `load`, but an image opened from a folder also gets its sibling images (Left/Right to browse),
/// like the Windows Photos app. Used for the file you press Space on, not for entries inside folder/archive views.
pub fn load_top(path: &Path, ctx: &egui::Context) -> Loaded {
    let loaded = load(path, ctx);
    if !is_gallery_image(path) {
        return loaded;
    }
    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let Ok(read_dir) = std::fs::read_dir(&dir) else { return loaded };
    let mut files: Vec<PathBuf> = read_dir
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && is_gallery_image(p))
        .collect();
    if files.len() < 2 {
        return loaded;
    }
    let name_of = |p: &Path| p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    files.sort_by(|a, b| natural_cmp(&name_of(a), &name_of(b)));
    let index = files.iter().position(|p| p.file_name() == path.file_name()).unwrap_or(0);
    Loaded::Gallery { first: Box::new(loaded), files, index }
}

/// SVG: rendered with resvg (pure Rust). Small icons are scaled up, huge drawings down.
fn load_svg(path: &Path) -> Loaded {
    const MAX_SIDE: f32 = 2048.0;
    const SMALL_SIDE: f32 = 1024.0;
    let result = (|| -> Result<egui::ColorImage, String> {
        let data = std::fs::read(path).map_err(|e| e.to_string())?;
        if data.len() > 64 << 20 {
            return Err("SVG file is too large".into());
        }
        let mut options = resvg::usvg::Options::default();
        options.resources_dir = path.parent().map(Path::to_path_buf);
        // Scanning system fonts is slow; only do it when the drawing has text.
        if data.windows(5).any(|w| w == b"<text") {
            std::sync::Arc::make_mut(&mut options.fontdb).load_system_fonts();
        }
        let tree = resvg::usvg::Tree::from_data(&data, &options).map_err(|e| e.to_string())?;
        let size = tree.size();
        let (w, h) = (size.width().max(1.0), size.height().max(1.0));
        let longest = w.max(h);
        let scale = if longest < SMALL_SIDE { SMALL_SIDE / longest } else { (MAX_SIDE / longest).min(1.0) };
        let (pw, ph) = (((w * scale).round() as u32).max(1), ((h * scale).round() as u32).max(1));
        let mut pixmap = resvg::tiny_skia::Pixmap::new(pw, ph).ok_or("SVG has no drawable size")?;
        pixmap.fill(resvg::tiny_skia::Color::from_rgba8(246, 246, 248, 255)); // light paper so dark strokes show
        resvg::render(&tree, resvg::tiny_skia::Transform::from_scale(scale, scale), &mut pixmap.as_mut());
        Ok(egui::ColorImage::from_rgba_premultiplied([pw as usize, ph as usize], pixmap.data()))
    })();
    match result {
        Ok(image) => Loaded::Image(image),
        Err(e) => Loaded::Failed(e),
    }
}

fn load_pdf(path: &Path, ctx: &egui::Context, password: Option<&str>) -> Loaded {
    match pdf::open(path, ctx, password) {
        Ok(p) => Loaded::Pdf(p),
        Err(PdfError::Locked) => Loaded::PdfLocked { path: path.to_path_buf() },
        Err(PdfError::Other(e)) => Loaded::Failed(e),
    }
}

fn has_pdf_header(path: &Path) -> bool {
    let mut head = [0u8; 1024];
    let Ok(mut file) = File::open(path) else { return false };
    let n = file.read(&mut head).unwrap_or(0);
    head[..n].windows(5).any(|w| w == b"%PDF-")
}

/// Photoshop: the merged preview stored in the file (needs "Maximize Compatibility"), or, if that
/// is blank, the visible layers flattened.
fn load_psd(path: &Path) -> Loaded {
    const MAX_FILE_BYTES: u64 = 512 << 20;
    const MAX_PIXELS: u64 = 120_000_000;
    let result = (|| -> Result<egui::ColorImage, String> {
        let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
        if meta.len() > MAX_FILE_BYTES {
            return Err("PSD is too large to preview".into());
        }
        let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
        let doc = psd::Psd::from_bytes(&bytes).map_err(|e| format!("{e:?}"))?;
        let (w, h) = (doc.width(), doc.height());
        if (w as u64) * (h as u64) > MAX_PIXELS {
            return Err(format!("PSD is too large to preview ({w} × {h})"));
        }
        let mut rgba = doc.rgba();
        let blank = rgba.iter().skip(3).step_by(4).all(|&a| a == 0);
        if blank {
            if let Ok(flat) = doc.flatten_layers_rgba(&|_| true) {
                rgba = flat;
            }
        }
        let buffer = image::RgbaImage::from_raw(w, h, rgba).ok_or("unexpected pixel data in PSD")?;
        let mut img = image::DynamicImage::ImageRgba8(buffer);
        if img.width() > MAX_IMAGE_SIDE || img.height() > MAX_IMAGE_SIDE {
            img = img.resize(MAX_IMAGE_SIDE, MAX_IMAGE_SIDE, image::imageops::FilterType::Triangle);
        }
        let rgba = img.to_rgba8();
        let size = [rgba.width() as usize, rgba.height() as usize];
        Ok(egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw()))
    })();
    match result {
        Ok(image) => Loaded::Image(image),
        Err(e) => Loaded::Failed(e),
    }
}

fn load_image(path: &Path) -> Loaded {
    let result = (|| -> Result<egui::ColorImage, Box<dyn std::error::Error>> {
        let mut img = image::ImageReader::open(path)?.with_guessed_format()?.decode()?;
        if img.width() > MAX_IMAGE_SIDE || img.height() > MAX_IMAGE_SIDE {
            img = img.resize(MAX_IMAGE_SIDE, MAX_IMAGE_SIDE, image::imageops::FilterType::Triangle);
        }
        let rgba = img.to_rgba8();
        let size = [rgba.width() as usize, rgba.height() as usize];
        Ok(egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw()))
    })();
    match result {
        Ok(image) => Loaded::Image(image),
        Err(e) => Loaded::Failed(e.to_string()),
    }
}

fn load_text(path: &Path) -> Loaded {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) => return Loaded::Failed(e.to_string()),
    };
    let total = file.metadata().map(|m| m.len()).unwrap_or(0);
    let mut buf = Vec::new();
    if let Err(e) = file.take(MAX_TEXT_BYTES).read_to_end(&mut buf) {
        return Loaded::Failed(e.to_string());
    }

    // Binary sniff: NUL bytes or lots of invalid UTF-8 means "not text".
    if buf[..buf.len().min(8192)].contains(&0) {
        return Loaded::Unsupported("No preview for this file type yet".into());
    }
    let text = String::from_utf8_lossy(&buf);
    if text.chars().take(8192).filter(|c| *c == '\u{FFFD}').count() > 8 {
        return Loaded::Unsupported("No preview for this file type yet".into());
    }

    let mut lines: Vec<String> = text
        .lines()
        .take(MAX_TEXT_LINES + 1)
        .map(|l| l.replace('\t', "    "))
        .collect();
    let mut truncated = total > MAX_TEXT_BYTES;
    if lines.len() > MAX_TEXT_LINES {
        lines.truncate(MAX_TEXT_LINES);
        truncated = true;
    }
    Loaded::Text { lines, truncated }
}

// ------------------------------------------------------------------------------------------
// Pane: a slot that loads in the background and then shows the result.
// The window uses one; an archive uses one for the selected entry.
// ------------------------------------------------------------------------------------------

pub struct Pane {
    rx: Option<Receiver<Loaded>>,
    pub content: Content,
}

impl Pane {
    pub fn spawn(ctx: &egui::Context, job: impl FnOnce() -> Loaded + Send + 'static) -> Self {
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let loaded = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job))
                .unwrap_or_else(|_| Loaded::Failed("The decoder crashed on this file".into()));
            let _ = tx.send(loaded);
            ctx.request_repaint();
        });
        Self { rx: Some(rx), content: Content::Loading }
    }

    /// A pane that already holds its content.
    pub fn ready(content: Content) -> Self {
        Self { rx: None, content }
    }

    /// Pick up the loaded result if it has arrived.
    pub fn poll(&mut self, ctx: &egui::Context) {
        let arrived = self.rx.as_ref().and_then(|rx| rx.try_recv().ok());
        if let Some(loaded) = arrived {
            self.content = Content::from_loaded(ctx, loaded);
            self.rx = None;
        }
    }

    pub fn is_loading(&self) -> bool {
        self.rx.is_some()
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        self.poll(&ctx);
        self.content.ui(ui);
    }
}

// ------------------------------------------------------------------------------------------
// Content
// ------------------------------------------------------------------------------------------

/// One item in the status bar: an icon and a short text.
pub struct Chip {
    pub icon: &'static str,
    pub text: String,
}

fn chip(icon: &'static str, text: impl Into<String>) -> Chip {
    Chip { icon, text: text.into() }
}

pub enum Content {
    Loading,
    Image(ImageView),
    Text { lines: Vec<String>, truncated: bool },
    Html(Box<HtmlView>),
    Markdown(Box<MarkdownView>),
    Pdf(Box<PdfView>),
    PdfLocked(Box<PdfLockView>),
    Folder(Box<FolderView>),
    Table(TableView),
    Doc(DocView),
    Slides(SlidesView),
    Archive(Box<ArchiveView>),
    Media(Box<MediaView>),
    Model(Box<ModelView>),
    Gallery(Box<GalleryView>),
    Unsupported(String),
    Failed(String),
}

impl Content {
    fn from_loaded(ctx: &egui::Context, loaded: Loaded) -> Self {
        match loaded {
            Loaded::Image(img) => {
                let size = img.size;
                let tex = ctx.load_texture("preview", img, egui::TextureOptions::LINEAR);
                Content::Image(ImageView { tex, size, zoom: 1.0, pan: egui::Vec2::ZERO, scale: 1.0 })
            }
            Loaded::Text { lines, truncated } => Content::Text { lines, truncated },
            Loaded::Html { path, lines, truncated } => Content::Html(Box::new(HtmlView::new(path, lines, truncated))),
            Loaded::Markdown { path, doc, lines, truncated } => {
                Content::Markdown(Box::new(MarkdownView::new(ctx, path, doc, lines, truncated)))
            }
            Loaded::Pdf(p) => Content::Pdf(Box::new(PdfView::new(p))),
            Loaded::PdfLocked { path } => Content::PdfLocked(Box::new(PdfLockView::new(path))),
            Loaded::Folder(listing) => Content::Folder(Box::new(FolderView::new(ctx, listing))),
            Loaded::Table(sheets) => Content::Table(TableView { sheets, active: 0 }),
            Loaded::Doc(blocks) => Content::Doc(DocView::new(blocks)),
            Loaded::Slides(slides) => {
                Content::Slides(SlidesView { slides, current: 0, last_flip: 0.0, pager_clicks: (false, false) })
            }
            Loaded::Archive(listing) => Content::Archive(Box::new(ArchiveView::new(ctx, listing))),
            Loaded::Gallery { first, files, index } => {
                Content::Gallery(Box::new(GalleryView::new(ctx, *first, files, index)))
            }
            Loaded::Media { path, .. } => match MediaView::new(&path, ctx) {
                Ok(view) => Content::Media(Box::new(view)),
                Err(e) => Content::Failed(e),
            },
            Loaded::Model(data) => Content::Model(Box::new(ModelView::new(*data))),
            Loaded::Unsupported(msg) => Content::Unsupported(msg),
            Loaded::Failed(e) => Content::Failed(e),
        }
    }

    /// The content sits next to a file list (folder / archive view) that owns the arrow keys.
    pub fn set_embedded(&mut self) {
        if let Content::Model(m) = self {
            m.set_embedded();
        }
    }

    /// The window shape the content wants (audio: compact player; video: the picture's shape).
    pub fn window_hint(&self) -> Option<WindowHint> {
        match self {
            Content::Media(m) => m.window_hint(),
            _ => None,
        }
    }

    /// When browsing images, the title bar and status bar follow the current image: (file name, size in bytes).
    pub fn title_override(&self) -> Option<(String, u64)> {
        match self {
            Content::Gallery(g) => Some((g.name(), g.size)),
            _ => None,
        }
    }

    /// Items for the status bar (dimensions, page count, duration, ...).
    pub fn chips(&self) -> Vec<Chip> {
        match self {
            Content::Image(v) => vec![
                chip(icons::ARROWS_OUT, format!("{} × {}", v.size[0], v.size[1])),
                chip(icons::MAGNIFYING_GLASS, format!("{:.0}%", v.scale * 100.0)),
            ],
            Content::Text { lines, truncated } => {
                let mut chips = vec![chip(icons::LIST_NUMBERS, format!("{} lines", lines.len()))];
                if *truncated {
                    chips.push(chip(icons::WARNING, "Truncated"));
                }
                chips
            }
            Content::Html(h) => h.chips(),
            Content::Markdown(m) => m.chips(),
            Content::Pdf(v) => vec![chip(icons::FILE_TEXT, format!("{} pages", v.page_count))],
            Content::PdfLocked(_) => vec![chip(icons::LOCK, "Password required")],
            Content::Table(t) => t.chips(),
            Content::Doc(d) => vec![chip(icons::FILE_TEXT, format!("~{} pages", d.page_count()))],
            Content::Slides(s) => vec![chip(icons::PRESENTATION_CHART, format!("{} slides", s.slides.len()))],
            Content::Media(m) => vec![chip(icons::CLOCK, m.status())],
            Content::Model(m) => m.chips().into_iter().map(|(icon, text)| chip(icon, text)).collect(),
            Content::Gallery(g) => g.current.content.chips(),
            Content::Archive(a) => {
                let mut chips = vec![chip(
                    icons::STACK,
                    format!("{} items{}", a.listing.entries.len(), if a.listing.truncated { "+" } else { "" }),
                )];
                if let Some(pane) = &a.child {
                    chips.extend(pane.content.chips());
                }
                chips
            }
            Content::Folder(f) => {
                let mut chips = vec![chip(
                    icons::FOLDER,
                    format!("{} items{}", f.listing.entries.len(), if f.listing.truncated { "+" } else { "" }),
                )];
                if let Some(pane) = &f.child {
                    chips.extend(pane.content.chips());
                }
                chips
            }
            _ => Vec::new(),
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        match self {
            Content::Loading => {
                ui.centered_and_justified(|ui| ui.label(RichText::new("Loading…").weak()));
            }
            Content::Image(view) => view.ui(ui),
            Content::Text { lines, .. } => text_ui(ui, lines),
            Content::Html(html) => html.ui(ui),
            Content::Markdown(md) => md.ui(ui),
            Content::Pdf(view) => view.ui(ui),
            Content::PdfLocked(lock) => {
                // Once a password works, this view replaces itself with the PDF.
                let replacement = lock.ui(ui);
                if let Some(content) = replacement {
                    *self = content;
                }
            }
            Content::Folder(folder) => folder.ui(ui),
            Content::Table(table) => table.ui(ui),
            Content::Doc(doc) => doc.ui(ui),
            Content::Slides(slides) => slides.ui(ui),
            Content::Archive(archive) => archive.ui(ui),
            Content::Media(media) => media.ui(ui),
            Content::Model(model) => model.ui(ui),
            Content::Gallery(gallery) => gallery.ui(ui),
            Content::Unsupported(msg) => notice(ui, icons::FILE, Color32::from_gray(150), msg, None),
            Content::Failed(error) => notice(
                ui,
                icons::WARNING,
                Color32::from_rgb(240, 170, 80),
                "Can't preview this file",
                Some(error.as_str()),
            ),
        }
    }
}

/// Centred icon + message (unsupported, errors).
fn notice(ui: &mut egui::Ui, glyph: &str, color: Color32, title: &str, detail: Option<&str>) {
    ui.vertical_centered(|ui| {
        ui.add_space((ui.available_height() * 0.28).max(8.0));
        ui.label(theme::icon_text(glyph, 40.0).color(color));
        ui.add_space(8.0);
        ui.label(RichText::new(title).size(17.0));
        if let Some(detail) = detail {
            ui.add_space(4.0);
            ui.add(egui::Label::new(RichText::new(detail).weak()).wrap_mode(egui::TextWrapMode::Wrap));
        }
    });
}

// ---- shared: page keys ----

/// Keys that mean "previous / next page / first / last".
fn page_keys(ctx: &egui::Context) -> (bool, bool, bool, bool) {
    ctx.input(|i| {
        (
            i.key_pressed(egui::Key::PageUp) || i.key_pressed(egui::Key::ArrowLeft),
            i.key_pressed(egui::Key::PageDown) || i.key_pressed(egui::Key::ArrowRight),
            i.key_pressed(egui::Key::Home),
            i.key_pressed(egui::Key::End),
        )
    })
}

// ---- images with zoom and pan ----

pub struct ImageView {
    tex: egui::TextureHandle,
    size: [usize; 2],
    /// 1.0 = fit to window.
    zoom: f32,
    pan: egui::Vec2,
    /// Actual on-screen scale relative to the image's pixels (for the footer).
    scale: f32,
}

const MIN_ZOOM: f32 = 0.2;
const MAX_SCALE: f32 = 32.0;

impl ImageView {
    fn ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let (rect, response) = ui.allocate_exact_size(ui.available_size(), Sense::click_and_drag());
        let tex_size = self.tex.size_vec2();
        // Never upscale small images when "fitting".
        let fit = (rect.width() / tex_size.x).min(rect.height() / tex_size.y).min(1.0);
        let hovered = ui.rect_contains_pointer(rect);
        let cursor = ctx.input(|i| i.pointer.hover_pos()).filter(|_| hovered).unwrap_or(rect.center());

        // Zoom factor from: pinch / Ctrl+wheel (egui's zoom_delta), Alt+wheel, Alt/Ctrl + "+" / "-".
        let mut factor = 1.0f32;
        let mut reset = false;
        let mut scroll_pan = egui::Vec2::ZERO;
        ctx.input(|i| {
            let modifier = i.modifiers.alt || i.modifiers.ctrl;
            if hovered {
                factor *= i.zoom_delta();
                if i.modifiers.alt {
                    factor *= (i.smooth_scroll_delta.y / 250.0).exp();
                } else if !i.modifiers.ctrl {
                    scroll_pan = i.smooth_scroll_delta;
                }
            }
            if modifier {
                if i.key_pressed(egui::Key::Plus) || i.key_pressed(egui::Key::Equals) {
                    factor *= 1.25;
                }
                if i.key_pressed(egui::Key::Minus) {
                    factor /= 1.25;
                }
                if i.key_pressed(egui::Key::Num0) {
                    reset = true;
                }
            }
        });
        // Keyboard zoom goes to the window centre, mouse/touchpad zoom to the cursor.
        let anchor = if hovered { cursor } else { rect.center() };

        if reset {
            self.zoom = 1.0;
            self.pan = egui::Vec2::ZERO;
        }
        if (factor - 1.0).abs() > 1e-4 {
            let new_zoom = (self.zoom * factor).clamp(MIN_ZOOM, MAX_SCALE / fit);
            let ratio = new_zoom / self.zoom;
            let c = anchor - rect.center();
            self.pan = c - (c - self.pan) * ratio; // keep the point under the cursor fixed
            self.zoom = new_zoom;
        }
        if self.zoom > 1.001 {
            self.pan += scroll_pan; // plain wheel / two-finger scroll pans when zoomed in
        }
        if response.dragged() {
            self.pan += response.drag_delta();
        }
        if response.double_clicked() {
            if (self.zoom - 1.0).abs() < 0.01 {
                self.zoom = (1.0 / fit).max(2.0).min(MAX_SCALE / fit); // actual pixels
            } else {
                self.zoom = 1.0;
                self.pan = egui::Vec2::ZERO;
            }
        }

        self.scale = fit * self.zoom;
        let shown = tex_size * self.scale;
        let limit = ((shown - rect.size()) / 2.0).max(egui::Vec2::ZERO);
        self.pan = vec2(self.pan.x.clamp(-limit.x, limit.x), self.pan.y.clamp(-limit.y, limit.y));

        let image_rect = Rect::from_center_size(rect.center() + self.pan, shown);
        let painter = ui.painter().with_clip_rect(rect);
        painter.image(
            self.tex.id(),
            image_rect,
            Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
            Color32::WHITE,
        );
        if (self.zoom - 1.0).abs() > 0.01 {
            painter.text(
                rect.right_bottom() - vec2(10.0, 8.0),
                Align2::RIGHT_BOTTOM,
                format!("{:.0}%", self.scale * 100.0),
                FontId::proportional(14.0),
                Color32::from_white_alpha(200),
            );
            if hovered {
                ctx.set_cursor_icon(egui::CursorIcon::Grab);
            }
        }
    }
}

// ---- text ----

pub(crate) fn text_ui(ui: &mut egui::Ui, lines: &[String]) {
    if lines.is_empty() {
        ui.centered_and_justified(|ui| ui.weak("(empty file)"));
        return;
    }
    let row_height = ui.text_style_height(&TextStyle::Monospace);
    egui::ScrollArea::both()
        .auto_shrink([false, false])
        .show_rows(ui, row_height, lines.len(), |ui, range| {
            for line in &lines[range] {
                ui.add(egui::Label::new(RichText::new(line).monospace()).wrap_mode(egui::TextWrapMode::Extend));
            }
        });
}

// ---- PDF: continuous scrolling, pages rendered on demand ----

const PAGE_GAP: f32 = 10.0;
const MAX_CACHED_PAGES: usize = 12;

pub struct PdfView {
    page_count: u32,
    /// Height / width per page (page 0 is exact, the rest are filled in by the worker).
    aspects: Vec<f32>,
    textures: HashMap<u32, egui::TextureHandle>,
    last_requested: Vec<u32>,
    req_tx: Sender<Vec<u32>>,
    resp_rx: Receiver<PdfMsg>,
    current: u32,
    offset_y: f32,
    jump_to: Option<f32>,
    pager_clicks: (bool, bool),
}

impl PdfView {
    fn new(p: PdfLoaded) -> Self {
        Self {
            page_count: p.page_count,
            aspects: vec![p.first_aspect; p.page_count as usize],
            textures: HashMap::new(),
            last_requested: Vec::new(),
            req_tx: p.req_tx,
            resp_rx: p.resp_rx,
            current: 0,
            offset_y: 0.0,
            jump_to: None,
            pager_clicks: (false, false),
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let area_rect = ui.max_rect();

        while let Ok(msg) = self.resp_rx.try_recv() {
            match msg {
                PdfMsg::Sizes { start, aspects } => {
                    for (k, aspect) in aspects.into_iter().enumerate() {
                        if let Some(slot) = self.aspects.get_mut(start as usize + k) {
                            *slot = aspect;
                        }
                    }
                }
                PdfMsg::Page(index, image) => {
                    let tex = ctx.load_texture(format!("pdf-page-{index}"), image, egui::TextureOptions::LINEAR);
                    self.textures.insert(index, tex);
                }
            }
        }

        // Layout: every page's top edge in scroll-content coordinates.
        let page_w = (ui.available_width() - 28.0).clamp(200.0, 1100.0);
        let mut tops = Vec::with_capacity(self.aspects.len());
        let mut y = PAGE_GAP;
        for aspect in &self.aspects {
            tops.push(y);
            y += page_w * aspect + PAGE_GAP;
        }
        let total_height = y;

        // Keys and last frame's clicks on the page pill (drawn over the pages below).
        let (prev, next) = std::mem::take(&mut self.pager_clicks);
        let (k_prev, k_next, k_first, k_last) = page_keys(&ctx);
        let last_page = self.page_count - 1;
        let goto = if next || k_next {
            Some((self.current + 1).min(last_page))
        } else if prev || k_prev {
            Some(self.current.saturating_sub(1))
        } else if k_first {
            Some(0)
        } else if k_last {
            Some(last_page)
        } else {
            None
        };
        if let Some(page) = goto {
            self.jump_to = Some((tops[page as usize] - PAGE_GAP).max(0.0));
        }

        let mut area = egui::ScrollArea::vertical().auto_shrink([false, false]);
        if let Some(offset) = self.jump_to.take() {
            area = area.vertical_scroll_offset(offset);
        }
        let aspects = &self.aspects;
        let textures = &self.textures;
        let output = area.show_viewport(ui, |ui, viewport| {
            ui.set_height(total_height);
            let origin = ui.min_rect().min;
            let x0 = origin.x + ((ui.available_width() - page_w) / 2.0).max(0.0);
            let mut wanted: Vec<u32> = Vec::new();
            for (i, &top) in tops.iter().enumerate() {
                let height = page_w * aspects[i];
                // Visible, or within ~one page of being visible (so it's ready when scrolled to).
                if top + height < viewport.min.y - 500.0 || top > viewport.max.y + 500.0 {
                    continue;
                }
                wanted.push(i as u32);
                let rect = Rect::from_min_size(pos2(x0, origin.y + top), vec2(page_w, height));
                match textures.get(&(i as u32)) {
                    Some(tex) => {
                        ui.painter().image(
                            tex.id(),
                            rect,
                            Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                            Color32::WHITE,
                        );
                    }
                    None => {
                        ui.painter().rect_filled(rect, 2.0, Color32::from_gray(245));
                        ui.painter().text(
                            rect.center(),
                            Align2::CENTER_CENTER,
                            "Rendering…",
                            FontId::proportional(14.0),
                            Color32::DARK_GRAY,
                        );
                    }
                }
            }
            wanted
        });

        self.offset_y = output.state.offset.y;
        let view_h = output.inner_rect.height();
        let probe = self.offset_y + view_h * 0.35;
        self.current = tops.iter().rposition(|&t| t - PAGE_GAP / 2.0 <= probe).unwrap_or(0) as u32;

        // Ask the worker for pages we can see but don't have, nearest first.
        let wanted = output.inner;
        let centre = self.offset_y + view_h / 2.0;
        let mut missing: Vec<u32> = wanted.iter().copied().filter(|i| !self.textures.contains_key(i)).collect();
        missing.sort_by(|a, b| {
            let dist = |i: &u32| (tops[*i as usize] + page_w * self.aspects[*i as usize] / 2.0 - centre).abs();
            dist(a).total_cmp(&dist(b))
        });
        if missing != self.last_requested {
            let _ = self.req_tx.send(missing.clone());
            self.last_requested = missing;
        }

        let (clicked_prev, clicked_next) = theme::floating_pager(
            &ctx,
            ui.id().with("pdf_pager"),
            area_rect,
            &format!("{} / {}", self.current + 1, self.page_count),
            self.current > 0,
            self.current + 1 < self.page_count,
        );
        self.pager_clicks = (clicked_prev, clicked_next);
        if clicked_prev || clicked_next {
            ctx.request_repaint();
        }

        // Keep memory bounded: drop far-away pages.
        if self.textures.len() > MAX_CACHED_PAGES {
            let keep: HashSet<u32> = wanted.into_iter().collect();
            let current = self.current;
            self.textures.retain(|k, _| keep.contains(k) || k.abs_diff(current) <= 4);
        }
    }
}

// ---- spreadsheets ----

pub struct TableView {
    sheets: Vec<Sheet>,
    active: usize,
}

fn col_letter(mut n: usize) -> String {
    let mut s = String::new();
    loop {
        s.insert(0, (b'A' + (n % 26) as u8) as char);
        if n < 26 {
            break;
        }
        n = n / 26 - 1;
    }
    s
}

impl TableView {
    fn chips(&self) -> Vec<Chip> {
        let Some(sheet) = self.sheets.get(self.active) else { return Vec::new() };
        vec![
            chip(icons::TABLE, sheet.name.clone()),
            chip(
                icons::LIST_NUMBERS,
                format!("{} × {}{}", sheet.rows.len(), sheet.ncols, if sheet.truncated { "+" } else { "" }),
            ),
        ]
    }

    fn ui(&mut self, ui: &mut egui::Ui) {
        if self.sheets.len() > 1 {
            ui.horizontal_wrapped(|ui| {
                for (i, sheet) in self.sheets.iter().enumerate() {
                    ui.selectable_value(&mut self.active, i, &sheet.name);
                }
            });
            ui.separator();
        }
        let Some(sheet) = self.sheets.get(self.active) else { return };
        if sheet.rows.is_empty() {
            ui.centered_and_justified(|ui| ui.weak("(empty sheet)"));
            return;
        }

        ui.push_id(("sheet", self.active), |ui| {
            egui::ScrollArea::horizontal().auto_shrink([false, false]).show(ui, |ui| {
                let mut builder = TableBuilder::new(ui)
                    .striped(true)
                    .resizable(true)
                    .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                    .column(Column::exact(52.0));
                for _ in 0..sheet.ncols {
                    builder = builder.column(Column::initial(120.0).at_least(40.0).clip(true));
                }
                builder
                    .header(22.0, |mut header| {
                        header.col(|ui| {
                            ui.label("");
                        });
                        for c in 0..sheet.ncols {
                            header.col(|ui| {
                                ui.strong(col_letter(sheet.col0 as usize + c));
                            });
                        }
                    })
                    .body(|body| {
                        body.rows(20.0, sheet.rows.len(), |mut row| {
                            let r = row.index();
                            row.col(|ui| {
                                ui.weak((sheet.row0 as usize + r + 1).to_string());
                            });
                            for c in 0..sheet.ncols {
                                row.col(|ui| {
                                    if let Some(value) = sheet.rows[r].get(c) {
                                        ui.add(egui::Label::new(value).wrap_mode(egui::TextWrapMode::Truncate));
                                    }
                                });
                            }
                        });
                    });
            });
        });
    }
}

// ---- Word (simplified): continuous scroll, with approximate "pages" (one screenful each) ----

pub struct DocView {
    blocks: Vec<Block>,
    offset_y: f32,
    view_h: f32,
    total_h: f32,
    jump_to: Option<f32>,
    pager_clicks: (bool, bool),
}

impl DocView {
    fn new(blocks: Vec<Block>) -> Self {
        Self { blocks, offset_y: 0.0, view_h: 1.0, total_h: 1.0, jump_to: None, pager_clicks: (false, false) }
    }

    fn page_count(&self) -> u32 {
        ((self.total_h / self.view_h.max(1.0)).ceil() as u32).max(1)
    }

    fn current_page(&self) -> u32 {
        let pages = self.page_count();
        if self.offset_y + self.view_h >= self.total_h - 2.0 {
            return pages;
        }
        ((self.offset_y / self.view_h.max(1.0)).floor() as u32 + 1).min(pages)
    }

    fn ui(&mut self, ui: &mut egui::Ui) {
        if self.blocks.is_empty() {
            notice(ui, icons::FILE_TEXT, Color32::from_gray(150), "No text found in this document", None);
            return;
        }
        let ctx = ui.ctx().clone();
        let area_rect = ui.max_rect();
        let (prev, next) = std::mem::take(&mut self.pager_clicks);
        let (k_prev, k_next, k_first, k_last) = page_keys(&ctx);
        let step = self.view_h * 0.9;
        if next || k_next {
            self.jump_to = Some(self.offset_y + step);
        } else if prev || k_prev {
            self.jump_to = Some((self.offset_y - step).max(0.0));
        } else if k_first {
            self.jump_to = Some(0.0);
        } else if k_last {
            self.jump_to = Some(self.total_h);
        }

        let mut area = egui::ScrollArea::vertical().auto_shrink([false, false]);
        if let Some(offset) = self.jump_to.take() {
            area = area.vertical_scroll_offset(offset);
        }
        let blocks = &self.blocks;
        let output = area.show(ui, |ui| {
            // A centred reading column.
            let side = ((ui.available_width() - 800.0) / 2.0).max(0.0);
            egui::Frame::none()
                .outer_margin(egui::Margin::symmetric(side, 10.0))
                .show(ui, |ui| {
                    for (i, block) in blocks.iter().enumerate() {
                        match block {
                            Block::Heading(level, text) => {
                                let size = match *level {
                                    1 => 26.0,
                                    2 => 22.0,
                                    3 => 19.0,
                                    _ => 17.0,
                                };
                                ui.add_space(8.0);
                                ui.label(RichText::new(text).size(size).strong());
                            }
                            Block::Para(text) => {
                                ui.label(text);
                            }
                            Block::Bullet(text) => {
                                ui.label(format!("•  {text}"));
                            }
                            Block::Table(rows) => {
                                egui::Grid::new(("docx_table", i)).striped(true).show(ui, |ui| {
                                    for row in rows {
                                        for cell in row {
                                            ui.label(cell);
                                        }
                                        ui.end_row();
                                    }
                                });
                            }
                        }
                        ui.add_space(4.0);
                    }
                });
        });
        self.offset_y = output.state.offset.y;
        self.view_h = output.inner_rect.height();
        self.total_h = output.content_size.y;

        let (current, pages) = (self.current_page(), self.page_count());
        let (clicked_prev, clicked_next) = theme::floating_pager(
            &ctx,
            ui.id().with("doc_pager"),
            area_rect,
            &format!("{current} / {pages}"),
            current > 1,
            current < pages,
        );
        self.pager_clicks = (clicked_prev, clicked_next);
        if clicked_prev || clicked_next {
            ctx.request_repaint();
        }
    }
}

// ---- PowerPoint (text only): one slide at a time ----

pub struct SlidesView {
    slides: Vec<Slide>,
    current: usize,
    last_flip: f64,
    pager_clicks: (bool, bool),
}

impl SlidesView {
    fn ui(&mut self, ui: &mut egui::Ui) {
        if self.slides.is_empty() {
            notice(ui, icons::PRESENTATION_CHART, Color32::from_gray(150), "No slides found", None);
            return;
        }
        let ctx = ui.ctx().clone();
        let area_rect = ui.max_rect();
        let last = self.slides.len() - 1;
        let (prev, next) = std::mem::take(&mut self.pager_clicks);
        let (k_prev, k_next, k_first, k_last) = page_keys(&ctx);

        // Mouse wheel / two-finger scroll flips slides (with a short cooldown so inertia doesn't skip).
        let (wheel, now) = ctx.input(|i| (i.raw_scroll_delta.y, i.time));
        let hovered = ui.rect_contains_pointer(area_rect);
        let mut wheel_next = false;
        let mut wheel_prev = false;
        if hovered && wheel.abs() > 1.0 && now - self.last_flip > 0.25 {
            wheel_next = wheel < 0.0;
            wheel_prev = wheel > 0.0;
        }

        let before = self.current;
        if next || k_next || wheel_next {
            self.current = (self.current + 1).min(last);
        } else if prev || k_prev || wheel_prev {
            self.current = self.current.saturating_sub(1);
        } else if k_first {
            self.current = 0;
        } else if k_last {
            self.current = last;
        }
        if self.current != before && (wheel_next || wheel_prev) {
            self.last_flip = now;
        }

        let slide = &self.slides[self.current];
        let side = ((ui.available_width() - 900.0) / 2.0).max(12.0);
        egui::Frame::none()
            .fill(theme::CARD_FILL)
            .rounding(egui::Rounding::same(14.0))
            .inner_margin(egui::Margin::same(30.0))
            .outer_margin(egui::Margin::symmetric(side, 10.0))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.set_min_height((ui.available_height() - 100.0).max(120.0));
                for (j, para) in slide.paras.iter().enumerate() {
                    if j == 0 {
                        ui.label(RichText::new(para.as_str()).size(28.0).strong());
                        ui.add_space(10.0);
                    } else {
                        ui.label(RichText::new(para.as_str()).size(16.0));
                    }
                }
            });

        let (clicked_prev, clicked_next) = theme::floating_pager(
            &ctx,
            ui.id().with("slides_pager"),
            area_rect,
            &format!("{} / {}", self.current + 1, self.slides.len()),
            self.current > 0,
            self.current < last,
        );
        self.pager_clicks = (clicked_prev, clicked_next);
        if clicked_prev || clicked_next {
            ctx.request_repaint();
        }
    }
}

// ---- archives ----

pub struct ArchiveView {
    listing: Listing,
    selected: Option<usize>,
    child: Option<Box<Pane>>,
    scroll_to: Option<usize>,
}

impl ArchiveView {
    fn new(ctx: &egui::Context, listing: Listing) -> Self {
        let mut view = Self { listing, selected: None, child: None, scroll_to: None };
        if let Some(first) = view.step(true) {
            view.select(ctx, first);
        }
        view
    }

    /// Next (or previous) file entry relative to the selection, skipping folders.
    fn step(&self, down: bool) -> Option<usize> {
        let entries = &self.listing.entries;
        match (self.selected, down) {
            (None, _) => entries.iter().position(|e| !e.is_dir),
            (Some(cur), true) => (cur + 1..entries.len()).find(|&i| !entries[i].is_dir),
            (Some(cur), false) => (0..cur).rev().find(|&i| !entries[i].is_dir),
        }
    }

    /// Extract the entry to a temp file on a background thread, then preview it like any other file.
    fn select(&mut self, ctx: &egui::Context, i: usize) {
        self.selected = Some(i);
        self.scroll_to = Some(i);
        let archive_path: PathBuf = self.listing.path.clone();
        let kind = self.listing.kind;
        let entry: Entry = self.listing.entries[i].clone();
        let job_ctx = ctx.clone();
        // Replacing the old pane drops it (this also stops any audio/video it was playing).
        self.child = None;
        self.child = Some(Box::new(Pane::spawn(ctx, move || {
            match archive::extract(&archive_path, kind, &entry) {
                Ok(tmp) => load(&tmp, &job_ctx),
                Err(e) => Loaded::Failed(e),
            }
        })));
    }

    fn ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let (down, up) = ctx.input(|i| (i.key_pressed(egui::Key::ArrowDown), i.key_pressed(egui::Key::ArrowUp)));
        let mut clicked: Option<usize> = if down || up { self.step(down) } else { None };

        let scroll_to = self.scroll_to.take();
        let selected = self.selected;
        let entries = &self.listing.entries;

        egui::SidePanel::left(ui.id().with("archive_list"))
            .frame(egui::Frame::none().fill(theme::PANEL_FILL).inner_margin(egui::Margin::same(6.0)))
            .resizable(true)
            .default_width(340.0)
            .width_range(200.0..=640.0)
            .show_inside(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                let mut area = egui::ScrollArea::vertical().auto_shrink([false, false]);
                if let Some(target) = scroll_to {
                    area = area.vertical_scroll_offset((target as f32 * (theme::ROW_HEIGHT + 2.0) - 120.0).max(0.0));
                }
                area.show_rows(ui, theme::ROW_HEIGHT, entries.len(), |ui, range| {
                    for i in range {
                        let entry = &entries[i];
                        if entry.is_dir {
                            theme::list_row(ui, false, icons::FOLDER_SIMPLE, entry.name.trim_end_matches('/'), "", true);
                        } else {
                            let response = theme::list_row(
                                ui,
                                selected == Some(i),
                                theme::file_icon(&entry.name),
                                &entry.name,
                                &human_size(entry.size),
                                false,
                            );
                            if response.clicked() {
                                clicked = Some(i);
                            }
                        }
                    }
                });
            });

        if let Some(i) = clicked {
            self.select(&ctx, i);
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::none().inner_margin(egui::Margin::same(6.0)))
            .show_inside(ui, |ui| match self.child.as_mut() {
                Some(pane) => {
                    pane.content.set_embedded();
                    pane.ui(ui)
                }
                None => notice(ui, icons::STACK, Color32::from_gray(150), "Nothing to preview in this archive", None),
            });
    }
}

// ---- password-protected PDF ----

pub struct PdfLockView {
    path: PathBuf,
    input: String,
    show: bool,
    remember: bool,
    recent: Vec<String>,
    error: Option<String>,
    focus_pending: bool,
    /// (result, the password that worked)
    attempt: Option<Receiver<(Loaded, Option<String>)>>,
}

fn mask(password: &str) -> String {
    let first: String = password.chars().take(1).collect();
    format!("{first}{}", "•".repeat(password.chars().count().saturating_sub(1)))
}

const CARD_WIDTH: f32 = 340.0;

impl PdfLockView {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            input: String::new(),
            show: false,
            remember: passwords::remember_by_default(),
            recent: passwords::recent(),
            error: None,
            focus_pending: true,
            attempt: None,
        }
    }

    /// Try the candidates in order on a background thread; stop at the first that opens the PDF.
    fn start(&mut self, ctx: &egui::Context, candidates: Vec<String>) {
        let (tx, rx) = mpsc::channel();
        let path = self.path.clone();
        let job_ctx = ctx.clone();
        std::thread::spawn(move || {
            let mut outcome = (Loaded::PdfLocked { path: path.clone() }, None);
            for password in candidates {
                match pdf::open(&path, &job_ctx, Some(&password)) {
                    Ok(opened) => {
                        outcome = (Loaded::Pdf(opened), Some(password));
                        break;
                    }
                    Err(PdfError::Locked) => continue,
                    Err(PdfError::Other(e)) => {
                        outcome = (Loaded::Failed(e), None);
                        break;
                    }
                }
            }
            let _ = tx.send(outcome);
            job_ctx.request_repaint();
        });
        self.attempt = Some(rx);
        self.error = None;
    }

    fn ui(&mut self, ui: &mut egui::Ui) -> Option<Content> {
        let ctx = ui.ctx().clone();

        let arrived = self.attempt.as_ref().and_then(|rx| rx.try_recv().ok());
        if let Some((loaded, used)) = arrived {
            self.attempt = None;
            match loaded {
                Loaded::PdfLocked { .. } => self.error = Some("Wrong password".into()),
                other => {
                    if self.remember {
                        if let Some(password) = used {
                            passwords::remember(&password);
                        }
                    }
                    return Some(Content::from_loaded(&ctx, other));
                }
            }
        }

        let busy = self.attempt.is_some();
        let file_name = self.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let mut submit: Option<Vec<String>> = None;

        ui.vertical_centered(|ui| {
            ui.add_space((ui.available_height() * 0.08).max(8.0));
            egui::Frame::none()
                .fill(theme::CARD_FILL)
                .rounding(egui::Rounding::same(16.0))
                .stroke(egui::Stroke::new(1.0, Color32::from_white_alpha(22)))
                .inner_margin(egui::Margin::same(26.0))
                .show(ui, |ui| {
                    ui.set_width(CARD_WIDTH);

                    ui.vertical_centered(|ui| {
                        ui.label(theme::icon_text(icons::LOCK, 40.0).color(theme::ACCENT));
                        ui.add_space(6.0);
                        ui.label(RichText::new("Password required").size(20.0).strong());
                        ui.add(
                            egui::Label::new(RichText::new(file_name.as_str()).weak())
                                .wrap_mode(egui::TextWrapMode::Truncate),
                        );
                    });
                    ui.add_space(14.0);

                    // The box is drawn by a frame with equal padding top and bottom, so the text sits in the middle.
                    let field = egui::Frame::none()
                        .fill(Color32::from_rgb(17, 17, 20))
                        .rounding(egui::Rounding::same(8.0))
                        .stroke(egui::Stroke::new(1.0, Color32::from_white_alpha(30)))
                        .inner_margin(egui::Margin::symmetric(12.0, 10.0))
                        .show(ui, |ui| {
                            ui.set_width(CARD_WIDTH - 24.0);
                            ui.add_enabled(
                                !busy,
                                egui::TextEdit::singleline(&mut self.input)
                                    .password(!self.show)
                                    .hint_text("Enter password")
                                    .frame(false)
                                    .desired_width(f32::INFINITY),
                            )
                        });
                    let edit = field.inner;
                    if edit.has_focus() {
                        ui.painter().rect_stroke(field.response.rect, 8.0, egui::Stroke::new(1.5, theme::ACCENT));
                    }
                    if self.focus_pending {
                        edit.request_focus();
                        self.focus_pending = false;
                    }
                    if edit.lost_focus() && ctx.input(|i| i.key_pressed(egui::Key::Enter)) && !self.input.is_empty() {
                        submit = Some(vec![self.input.clone()]);
                    }

                    ui.horizontal(|ui| {
                        ui.checkbox(&mut self.remember, "Remember");
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let (glyph, tip) = if self.show {
                                (icons::EYE_SLASH, "Hide password")
                            } else {
                                (icons::EYE, "Show password")
                            };
                            if theme::icon_button(ui, glyph, tip, false, true).clicked() {
                                self.show = !self.show;
                            }
                        });
                    });
                    ui.add_space(4.0);

                    let open = ui.add_enabled(
                        !busy && !self.input.is_empty(),
                        egui::Button::new(RichText::new("Open").size(15.0).strong())
                            .fill(theme::ACCENT.gamma_multiply(0.85))
                            .min_size(egui::vec2(CARD_WIDTH, 38.0)),
                    );
                    if open.clicked() {
                        submit = Some(vec![self.input.clone()]);
                    }

                    if !self.recent.is_empty() {
                        ui.add_space(10.0);
                        ui.separator();
                        ui.add_space(4.0);
                        ui.label(RichText::new("Recent passwords").small().weak());
                        for password in self.recent.clone() {
                            let label = if self.show { password.clone() } else { mask(&password) };
                            let button = egui::Button::new(label).min_size(egui::vec2(CARD_WIDTH, 32.0));
                            if ui.add_enabled(!busy, button).clicked() {
                                submit = Some(vec![password]);
                            }
                        }
                        ui.horizontal(|ui| {
                            let try_all = egui::Button::new("Try all saved").min_size(egui::vec2(CARD_WIDTH - 44.0, 30.0));
                            if ui.add_enabled(!busy, try_all).clicked() {
                                submit = Some(self.recent.clone());
                            }
                            if theme::icon_button(ui, icons::TRASH, "Forget saved passwords", false, true).clicked() {
                                passwords::forget_all();
                                self.recent.clear();
                            }
                        });
                    }

                    if busy || self.error.is_some() {
                        ui.add_space(8.0);
                        ui.vertical_centered(|ui| {
                            if busy {
                                ui.label(RichText::new("Trying…").weak());
                            }
                            if let Some(error) = &self.error {
                                ui.colored_label(Color32::from_rgb(255, 120, 120), error.as_str());
                            }
                        });
                    }
                });
        });

        if let Some(candidates) = submit {
            self.start(&ctx, candidates);
        }
        None
    }
}

// ---- folders ----

pub struct FolderView {
    listing: FolderListing,
    selected: Option<usize>,
    child: Option<Box<Pane>>,
    loading: Option<Receiver<Result<FolderListing, String>>>,
    scroll_to: Option<usize>,
    error: Option<String>,
}

impl FolderView {
    fn new(ctx: &egui::Context, listing: FolderListing) -> Self {
        let mut view = Self { listing, selected: None, child: None, loading: None, scroll_to: None, error: None };
        view.select_first_file(ctx);
        view
    }

    fn select_first_file(&mut self, ctx: &egui::Context) {
        self.selected = None;
        self.child = None;
        if let Some(i) = self.listing.entries.iter().position(|e| !e.is_dir) {
            self.select(ctx, i);
        }
    }

    fn select(&mut self, ctx: &egui::Context, i: usize) {
        self.selected = Some(i);
        self.scroll_to = Some(i);
        // Dropping the old pane also stops any audio/video it was playing.
        self.child = None;
        let entry = &self.listing.entries[i];
        if entry.is_dir {
            return;
        }
        let path = entry.path.clone();
        let job_ctx = ctx.clone();
        self.child = Some(Box::new(Pane::spawn(ctx, move || load(&path, &job_ctx))));
    }

    fn navigate(&mut self, ctx: &egui::Context, dir: PathBuf) {
        let (tx, rx) = mpsc::channel();
        let job_ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(folder::list(&dir));
            job_ctx.request_repaint();
        });
        self.loading = Some(rx);
        self.error = None;
        self.child = None;
    }

    fn step(&self, down: bool) -> Option<usize> {
        let n = self.listing.entries.len();
        if n == 0 {
            return None;
        }
        Some(match (self.selected, down) {
            (None, _) => 0,
            (Some(cur), true) => (cur + 1).min(n - 1),
            (Some(cur), false) => cur.saturating_sub(1),
        })
    }

    fn ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();

        let arrived = self.loading.as_ref().and_then(|rx| rx.try_recv().ok());
        if let Some(result) = arrived {
            self.loading = None;
            match result {
                Ok(listing) => {
                    self.listing = listing;
                    self.select_first_file(&ctx);
                }
                Err(e) => self.error = Some(e),
            }
        }

        // Up/Down select, Enter opens a selected folder, Backspace goes up (not while typing, e.g. a password).
        let typing = ctx.wants_keyboard_input();
        let (down, up, enter, back) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::ArrowDown),
                i.key_pressed(egui::Key::ArrowUp),
                i.key_pressed(egui::Key::Enter),
                i.key_pressed(egui::Key::Backspace),
            )
        });
        let mut clicked: Option<usize> = None;
        let mut open: Option<usize> = None;
        let mut go_up = false;
        if !typing && self.loading.is_none() {
            if down || up {
                clicked = self.step(down);
            }
            if enter {
                if let Some(i) = self.selected {
                    if self.listing.entries[i].is_dir {
                        open = Some(i);
                    }
                }
            }
            if back {
                go_up = true;
            }
        }

        let scroll_to = self.scroll_to.take();
        let selected = self.selected;
        let loading = self.loading.is_some();
        let error = self.error.clone();
        let parent = self.listing.dir.parent().map(Path::to_path_buf);
        let dir_text = self.listing.dir.display().to_string();
        let dir_name = self
            .listing
            .dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| dir_text.clone());
        let entries = &self.listing.entries;
        let dir_selected = selected.map(|i| entries[i].is_dir).unwrap_or(false);

        egui::SidePanel::left(ui.id().with("folder_list"))
            .frame(egui::Frame::none().fill(theme::PANEL_FILL).inner_margin(egui::Margin::same(6.0)))
            .resizable(true)
            .default_width(340.0)
            .width_range(200.0..=640.0)
            .show_inside(ui, |ui| {
                ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
                ui.horizontal(|ui| {
                    if theme::icon_button(ui, icons::ARROW_BEND_LEFT_UP, "Up one folder (Backspace)", false, parent.is_some())
                        .clicked()
                    {
                        go_up = true;
                    }
                    ui.label(theme::icon_text(icons::FOLDER_OPEN, 18.0).color(Color32::from_gray(170)));
                    ui.label(RichText::new(dir_name.as_str()).size(15.0).strong());
                });
                ui.label(RichText::new(dir_text.as_str()).small().weak());
                if loading {
                    ui.label(RichText::new("Loading…").weak());
                }
                if let Some(e) = &error {
                    ui.colored_label(Color32::from_rgb(255, 120, 120), e.as_str());
                }
                ui.add_space(2.0);

                ui.spacing_mut().item_spacing.y = 2.0;
                let mut area = egui::ScrollArea::vertical().auto_shrink([false, false]);
                if let Some(target) = scroll_to {
                    area = area.vertical_scroll_offset((target as f32 * (theme::ROW_HEIGHT + 2.0) - 120.0).max(0.0));
                }
                area.show_rows(ui, theme::ROW_HEIGHT, entries.len(), |ui, range| {
                    for i in range {
                        let entry = &entries[i];
                        let (glyph, size_text) = if entry.is_dir {
                            (icons::FOLDER_SIMPLE, String::new())
                        } else {
                            (theme::file_icon(&entry.name), human_size(entry.size))
                        };
                        let response = theme::list_row(ui, selected == Some(i), glyph, &entry.name, &size_text, entry.is_dir);
                        if response.clicked() {
                            clicked = Some(i);
                        }
                        if response.double_clicked() && entry.is_dir {
                            open = Some(i);
                        }
                    }
                });
            });

        if go_up {
            if let Some(dir) = parent {
                self.navigate(&ctx, dir);
            }
        } else if let Some(i) = open {
            let dir = self.listing.entries[i].path.clone();
            self.navigate(&ctx, dir);
        } else if let Some(i) = clicked {
            self.select(&ctx, i);
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::none().inner_margin(egui::Margin::same(6.0)))
            .show_inside(ui, |ui| match self.child.as_mut() {
                Some(pane) => {
                    pane.content.set_embedded();
                    pane.ui(ui)
                }
                None => {
                    let hint = if dir_selected {
                        "Folder: press Enter or double-click to open it"
                    } else {
                        "Nothing to preview here"
                    };
                    notice(ui, icons::FOLDER, Color32::from_gray(150), hint, None);
                }
            });
    }
}

// ---- browsing the images of a folder ----

pub struct GalleryView {
    files: Vec<PathBuf>,
    index: usize,
    current: Pane,
    /// The image being loaded after Left/Right; the old one stays on screen until it is ready.
    pending: Option<(usize, Pane)>,
    pager_clicks: (bool, bool),
    size: u64,
}

impl GalleryView {
    fn new(ctx: &egui::Context, first: Loaded, files: Vec<PathBuf>, index: usize) -> Self {
        let size = files.get(index).and_then(|p| std::fs::metadata(p).ok()).map(|m| m.len()).unwrap_or(0);
        Self {
            files,
            index,
            current: Pane::ready(Content::from_loaded(ctx, first)),
            pending: None,
            pager_clicks: (false, false),
            size,
        }
    }

    fn name(&self) -> String {
        self.files
            .get(self.index)
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    fn go(&mut self, ctx: &egui::Context, target: usize) {
        let path = self.files[target].clone();
        let job_ctx = ctx.clone();
        self.pending = Some((target, Pane::spawn(ctx, move || load(&path, &job_ctx))));
    }

    fn ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let area_rect = ui.max_rect();

        if let Some((target, mut pane)) = self.pending.take() {
            pane.poll(&ctx);
            if pane.is_loading() {
                self.pending = Some((target, pane));
            } else {
                self.current = pane;
                self.index = target;
                self.size = std::fs::metadata(&self.files[target]).map(|m| m.len()).unwrap_or(0);
            }
        }

        let n = self.files.len();
        let (prev_click, next_click) = std::mem::take(&mut self.pager_clicks);
        let typing = ctx.wants_keyboard_input();
        let (left, right) =
            ctx.input(|i| (i.key_pressed(egui::Key::ArrowLeft), i.key_pressed(egui::Key::ArrowRight)));
        // Keep going from where the last key press was heading, even while an image is still loading.
        let base = self.pending.as_ref().map(|(t, _)| *t).unwrap_or(self.index);
        if !typing && (right || next_click) {
            self.go(&ctx, (base + 1) % n);
        } else if !typing && (left || prev_click) {
            self.go(&ctx, (base + n - 1) % n);
        }
        if self.pending.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(20));
        }

        self.current.ui(ui);

        let shown = self.pending.as_ref().map(|(t, _)| *t).unwrap_or(self.index);
        let (clicked_prev, clicked_next) = theme::floating_pager(
            &ctx,
            ui.id().with("gallery_pager"),
            area_rect,
            &format!("{} / {}", shown + 1, n),
            true,
            true,
        );
        self.pager_clicks = (clicked_prev, clicked_next);
        if clicked_prev || clicked_next {
            ctx.request_repaint();
        }
    }
}

//! PDF through pdfium (pdfium-render), for Linux and other non-Windows systems.
//! libpdfium (libpdfium.so) is loaded at run time from the executable's folder, or the system.
//! One worker thread owns the document and renders the pages the UI asks for.

use std::{
    path::Path,
    sync::mpsc::{self, TryRecvError},
};

use eframe::egui;
use pdfium_render::prelude::*;

use super::{PdfError, PdfLoaded, PdfMsg};

const RENDER_WIDTH: i32 = 1400;
const SIZE_CHUNK: u32 = 40;

const MISSING: &str = "PDF support needs libpdfium (libpdfium.so). Put it next to the quicklook executable \
(https://github.com/bblanchon/pdfium-binaries) or install it system-wide.";

fn bind() -> Result<Pdfium, String> {
    let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf));
    if let Some(dir) = exe_dir {
        if let Ok(bindings) = Pdfium::bind_to_library(Pdfium::pdfium_platform_library_name_at_path(&dir)) {
            return Ok(Pdfium::new(bindings));
        }
    }
    Pdfium::bind_to_system_library()
        .map(Pdfium::new)
        .map_err(|_| MISSING.to_string())
}

fn aspect(pages: &PdfPages, index: u32) -> Option<f32> {
    let page = pages.get(index as u16).ok()?;
    let (w, h) = (page.width().value, page.height().value);
    (w > 0.0).then(|| h / w)
}

fn render(pages: &PdfPages, index: u32) -> Result<egui::ColorImage, String> {
    let page = pages.get(index as u16).map_err(|e| e.to_string())?;
    let config = PdfRenderConfig::new()
        .set_target_width(RENDER_WIDTH)
        .set_clear_color(PdfColor::WHITE)
        .set_format(PdfBitmapFormat::BGRA);
    let bitmap = page.render_with_config(&config).map_err(|e| e.to_string())?;
    let (w, h) = (bitmap.width() as usize, bitmap.height() as usize);
    let bytes = bitmap.as_raw_bytes();
    let mut rgba = Vec::with_capacity(w * h * 4);
    for px in bytes.chunks_exact(4) {
        rgba.extend_from_slice(&[px[2], px[1], px[0], 255]); // BGRA -> RGBA
    }
    Ok(egui::ColorImage::from_rgba_unmultiplied([w, h], &rgba))
}

pub fn open(path: &Path, ctx: &egui::Context, password: Option<&str>) -> Result<PdfLoaded, PdfError> {
    let (req_tx, req_rx) = mpsc::channel::<Vec<u32>>();
    let (resp_tx, resp_rx) = mpsc::channel::<PdfMsg>();
    let (init_tx, init_rx) = mpsc::channel::<Result<(u32, f32), PdfError>>();
    let path = path.to_path_buf();
    let password = password.map(str::to_string);
    let ctx = ctx.clone();

    std::thread::spawn(move || {
        let pdfium = match bind() {
            Ok(p) => p,
            Err(e) => {
                let _ = init_tx.send(Err(PdfError::Other(e)));
                return;
            }
        };
        let document = match pdfium.load_pdf_from_file(&path, password.as_deref()) {
            Ok(d) => d,
            Err(PdfiumError::PdfiumLibraryInternalError(PdfiumInternalError::PasswordError)) => {
                let _ = init_tx.send(Err(PdfError::Locked));
                return;
            }
            Err(e) => {
                let _ = init_tx.send(Err(PdfError::Other(e.to_string())));
                return;
            }
        };
        let pages = document.pages();
        let count = (pages.len() as u32).min(u16::MAX as u32);
        let first_aspect = aspect(&pages, 0).unwrap_or(1.414);
        let _ = init_tx.send(Ok((count, first_aspect)));

        let mut queue: Vec<u32> = Vec::new();
        let mut next_size = 1u32;
        loop {
            let idle_work = next_size < count;
            if queue.is_empty() && !idle_work {
                match req_rx.recv() {
                    Ok(list) => queue = list,
                    Err(_) => return,
                }
            }
            loop {
                match req_rx.try_recv() {
                    Ok(list) => queue = list, // newest request wins
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return,
                }
            }

            if !queue.is_empty() {
                let page = queue.remove(0);
                if let Ok(image) = render(&pages, page) {
                    if resp_tx.send(PdfMsg::Page(page, image)).is_err() {
                        return;
                    }
                    ctx.request_repaint();
                }
            } else if next_size < count {
                let end = (next_size + SIZE_CHUNK).min(count);
                let aspects: Vec<f32> = (next_size..end).map(|i| aspect(&pages, i).unwrap_or(1.414)).collect();
                if resp_tx.send(PdfMsg::Sizes { start: next_size, aspects }).is_err() {
                    return;
                }
                next_size = end;
                ctx.request_repaint();
            }
        }
    });

    let (page_count, first_aspect) = init_rx.recv().map_err(|e| PdfError::Other(e.to_string()))??;
    if page_count == 0 {
        return Err(PdfError::Other("PDF has no pages".into()));
    }
    Ok(PdfLoaded { page_count, first_aspect, req_tx, resp_rx })
}

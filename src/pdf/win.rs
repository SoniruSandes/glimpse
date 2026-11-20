//! PDF rendering through the Windows.Data.Pdf runtime API (built into Windows 10/11, no extra DLLs).
//! One worker thread owns the document. The UI sends "I need these pages" lists; the worker renders
//! the newest list first-to-last and, when idle, collects every page's size for the scroll layout.

use std::{
    path::Path,
    sync::mpsc::{self, TryRecvError},
};

use eframe::egui;
use windows::{
    core::{Interface, HSTRING},
    Data::Pdf::{PdfDocument, PdfPageRenderOptions},
    Storage::{
        Streams::{DataReader, IRandomAccessStream, InMemoryRandomAccessStream},
        StorageFile,
    },
    Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED},
    UI::Color,
};

const RENDER_WIDTH: u32 = 1400;
const SIZE_CHUNK: u32 = 40;

use super::{PdfError, PdfLoaded, PdfMsg};

pub fn open(path: &Path, ctx: &egui::Context, password: Option<&str>) -> Result<PdfLoaded, PdfError> {
    let (req_tx, req_rx) = mpsc::channel::<Vec<u32>>();
    let (resp_tx, resp_rx) = mpsc::channel::<PdfMsg>();
    let (init_tx, init_rx) = mpsc::channel::<Result<(u32, f32), PdfError>>();
    let password = password.map(str::to_string);
    let path = path.to_path_buf();
    let ctx = ctx.clone();

    std::thread::spawn(move || {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        let doc = match load_document(&path, password.as_deref()) {
            Ok(doc) => doc,
            Err(e) => {
                let _ = init_tx.send(Err(e));
                return;
            }
        };
        let count = doc.PageCount().unwrap_or(0);
        let first_aspect = page_aspect(&doc, 0).unwrap_or(1.414);
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
                if let Ok(image) = render_page(&doc, page) {
                    if resp_tx.send(PdfMsg::Page(page, image)).is_err() {
                        return;
                    }
                    ctx.request_repaint();
                }
            } else if next_size < count {
                let end = (next_size + SIZE_CHUNK).min(count);
                let aspects: Vec<f32> = (next_size..end).map(|i| page_aspect(&doc, i).unwrap_or(1.414)).collect();
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

fn werr(e: windows::core::Error) -> String {
    e.message()
}

/// ERROR_WRONG_PASSWORD (and E_ACCESSDENIED, which some Windows builds report instead).
fn classify(e: windows::core::Error) -> PdfError {
    match e.code().0 as u32 {
        0x8007_052B | 0x8007_0005 => PdfError::Locked,
        _ => PdfError::Other(e.message()),
    }
}

fn load_document(path: &Path, password: Option<&str>) -> Result<PdfDocument, PdfError> {
    let full = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let text = full.to_string_lossy().into_owned();
    let text = text.strip_prefix(r"\\?\").map(str::to_string).unwrap_or(text);

    let file = StorageFile::GetFileFromPathAsync(&HSTRING::from(text))
        .map_err(classify)?
        .get()
        .map_err(classify)?;
    let operation = match password {
        Some(pw) => PdfDocument::LoadFromFileWithPasswordAsync(&file, &HSTRING::from(pw)),
        None => PdfDocument::LoadFromFileAsync(&file),
    };
    operation.map_err(classify)?.get().map_err(classify)
}

fn page_aspect(doc: &PdfDocument, index: u32) -> Option<f32> {
    let size = doc.GetPage(index).ok()?.Size().ok()?;
    (size.Width > 0.0).then(|| size.Height / size.Width)
}

fn render_page(doc: &PdfDocument, index: u32) -> Result<egui::ColorImage, String> {
    let page = doc.GetPage(index).map_err(werr)?;
    let options = PdfPageRenderOptions::new().map_err(werr)?;
    options.SetDestinationWidth(RENDER_WIDTH).map_err(werr)?;
    options
        .SetBackgroundColor(Color { A: 255, R: 255, G: 255, B: 255 })
        .map_err(werr)?;

    let stream: IRandomAccessStream = InMemoryRandomAccessStream::new()
        .map_err(werr)?
        .cast()
        .map_err(werr)?;
    page.RenderWithOptionsToStreamAsync(&stream, &options)
        .map_err(werr)?
        .get()
        .map_err(werr)?;

    let size = stream.Size().map_err(werr)? as u32;
    let reader = DataReader::CreateDataReader(&stream.GetInputStreamAt(0).map_err(werr)?).map_err(werr)?;
    reader.LoadAsync(size).map_err(werr)?.get().map_err(werr)?;
    let mut bytes = vec![0u8; size as usize];
    reader.ReadBytes(&mut bytes).map_err(werr)?;

    let rgba = image::load_from_memory(&bytes).map_err(|e| e.to_string())?.to_rgba8();
    let dims = [rgba.width() as usize, rgba.height() as usize];
    Ok(egui::ColorImage::from_rgba_unmultiplied(dims, rgba.as_raw()))
}

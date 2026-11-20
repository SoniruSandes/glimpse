//! PDF support. Windows uses the built-in Windows.Data.Pdf renderer; Linux (and others) use pdfium,
//! loaded at run time (libpdfium next to the executable). Both speak the same small protocol:
//! the UI sends lists of page numbers it needs; the worker answers with sizes and rendered pages.

use std::sync::mpsc::{Receiver, Sender};

use eframe::egui;

pub enum PdfMsg {
    /// Height/width ratios for pages start .. start+len.
    Sizes { start: u32, aspects: Vec<f32> },
    Page(u32, egui::ColorImage),
}

pub struct PdfLoaded {
    pub page_count: u32,
    pub first_aspect: f32,
    pub req_tx: Sender<Vec<u32>>,
    pub resp_rx: Receiver<PdfMsg>,
}

pub enum PdfError {
    /// The PDF needs a password (or the one given was wrong).
    Locked,
    Other(String),
}

#[cfg(windows)]
mod win;
#[cfg(windows)]
pub use win::open;

#[cfg(not(windows))]
mod pdfium;
#[cfg(not(windows))]
pub use pdfium::open;

//! HTML files: code view by default, switchable to a live web view (Windows: WebView2 through wry).
//! On other systems the web view isn't available; "Open in browser" works everywhere.

use std::path::PathBuf;

use eframe::egui::{self, Color32, RichText, Sense};

use crate::{
    content::{text_ui, Chip},
    icons, theme,
};

// ------------------------------------------------------------------------------------------
// The web view itself (Windows only)
// ------------------------------------------------------------------------------------------

#[cfg(windows)]
mod web {
    use std::{
        num::NonZeroIsize,
        path::{Path, PathBuf},
    };

    use raw_window_handle::{HandleError, HasWindowHandle, RawWindowHandle, Win32WindowHandle, WindowHandle};

    pub const AVAILABLE: bool = true;

    /// Our preview window as a parent for the web view.
    struct Parent(isize);

    impl HasWindowHandle for Parent {
        fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
            let hwnd = NonZeroIsize::new(self.0).ok_or(HandleError::Unavailable)?;
            let raw = RawWindowHandle::Win32(Win32WindowHandle::new(hwnd));
            Ok(unsafe { WindowHandle::borrow_raw(raw) })
        }
    }

    pub struct WebPane {
        webview: wry::WebView,
        _context: wry::WebContext,
        placed: Option<(i32, i32, i32, i32)>,
        visible: bool,
    }

    fn bounds(x: i32, y: i32, w: i32, h: i32) -> wry::Rect {
        wry::Rect {
            position: wry::dpi::PhysicalPosition::new(x, y).into(),
            size: wry::dpi::PhysicalSize::new(w.max(1) as u32, h.max(1) as u32).into(),
        }
    }

    /// file:///C:/some%20folder/page.html
    fn file_url(path: &Path) -> String {
        let mut url = String::from("file:///");
        for byte in path.to_string_lossy().replace('\\', "/").bytes() {
            match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' | b':' => {
                    url.push(byte as char)
                }
                _ => url.push_str(&format!("%{byte:02X}")),
            }
        }
        url
    }

    impl WebPane {
        pub fn new(path: &Path, rect: (i32, i32, i32, i32)) -> Result<Self, String> {
            let parent = crate::window::window_hwnd();
            if parent == 0 {
                return Err("No window to draw the web view in".into());
            }
            let data_dir = std::env::var_os("LOCALAPPDATA").map(|p| PathBuf::from(p).join("quicklook-rs").join("webview"));
            let mut context = wry::WebContext::new(data_dir);
            let webview = wry::WebViewBuilder::new_with_web_context(&mut context)
                .with_url(file_url(path))
                .with_bounds(bounds(rect.0, rect.1, rect.2, rect.3))
                // Stay on local files: no navigating away to the internet from a preview.
                .with_navigation_handler(|url| {
                    url.starts_with("file:") || url.starts_with("about:") || url.starts_with("data:")
                })
                .build_as_child(&Parent(parent))
                .map_err(|e| format!("Web view unavailable ({e}). It needs the Microsoft WebView2 runtime."))?;
            Ok(Self { webview, _context: context, placed: Some(rect), visible: true })
        }

        pub fn show(&mut self, rect: (i32, i32, i32, i32)) {
            if !self.visible {
                let _ = self.webview.set_visible(true);
                self.visible = true;
            }
            if self.placed != Some(rect) {
                let _ = self.webview.set_bounds(bounds(rect.0, rect.1, rect.2, rect.3));
                self.placed = Some(rect);
            }
        }

        pub fn hide(&mut self) {
            if self.visible {
                let _ = self.webview.set_visible(false);
                self.visible = false;
            }
        }
    }
}

#[cfg(not(windows))]
mod web {
    use std::path::Path;

    pub const AVAILABLE: bool = false;

    pub struct WebPane;

    impl WebPane {
        pub fn new(_path: &Path, _rect: (i32, i32, i32, i32)) -> Result<Self, String> {
            Err("The web view is only available on Windows. Use \"Open in browser\".".into())
        }
        pub fn show(&mut self, _rect: (i32, i32, i32, i32)) {}
        pub fn hide(&mut self) {}
    }
}

use web::WebPane;

// ------------------------------------------------------------------------------------------

pub struct HtmlView {
    path: PathBuf,
    lines: Vec<String>,
    truncated: bool,
    web_mode: bool,
    web: Option<WebPane>,
    error: Option<String>,
    /// The web view takes a moment to start and blocks the window while it does:
    /// draw a "Starting…" frame first so the window doesn't just look frozen.
    starting_frames: u8,
}

impl HtmlView {
    pub fn new(path: PathBuf, lines: Vec<String>, truncated: bool) -> Self {
        Self { path, lines, truncated, web_mode: false, web: None, error: None, starting_frames: 0 }
    }

    pub fn chips(&self) -> Vec<Chip> {
        let mut chips = vec![Chip {
            icon: if self.web_mode { icons::GLOBE } else { icons::CODE },
            text: if self.web_mode { "Web view".into() } else { format!("{} lines", self.lines.len()) },
        }];
        if self.truncated && !self.web_mode {
            chips.push(Chip { icon: icons::WARNING, text: "Truncated".into() });
        }
        chips
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        let name = self.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        ui.horizontal(|ui| {
            let web_tip = if web::AVAILABLE { "Web view" } else { "Web view (Windows only)" };
            if theme::icon_button(ui, icons::EYE, web_tip, self.web_mode, web::AVAILABLE).clicked() {
                self.web_mode = true;
                self.starting_frames = 0;
            }
            if theme::icon_button(ui, icons::CODE, "Code view", !self.web_mode, true).clicked() {
                self.web_mode = false;
            }
            ui.add_space(6.0);
            ui.label(RichText::new(name).size(15.0));
            ui.label(RichText::new("HTML").weak());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if theme::icon_button(ui, icons::ARROW_SQUARE_OUT, "Open in browser", false, true).clicked() {
                    let _ = open::that(&self.path);
                }
            });
        });
        ui.add_space(2.0);

        if !self.web_mode {
            if let Some(web) = self.web.as_mut() {
                web.hide();
            }
            text_ui(ui, &self.lines);
            return;
        }

        let rect = ui.available_rect_before_wrap();
        ui.allocate_rect(rect, Sense::hover());
        let ctx = ui.ctx().clone();
        // Keep frames (and so click-away / drag handling) flowing while the web view is up.
        ctx.request_repaint_after(std::time::Duration::from_millis(150));

        let ppp = ctx.pixels_per_point();
        let px = (
            (rect.min.x * ppp).round() as i32,
            (rect.min.y * ppp).round() as i32,
            (rect.width() * ppp).round() as i32,
            (rect.height() * ppp).round() as i32,
        );
        if self.web.is_none() && self.error.is_none() {
            if self.starting_frames < 2 {
                self.starting_frames += 1;
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    "Starting web view…",
                    egui::FontId::proportional(16.0),
                    Color32::GRAY,
                );
                ctx.request_repaint();
                return;
            }
            match WebPane::new(&self.path, px) {
                Ok(web) => self.web = Some(web),
                Err(e) => self.error = Some(e),
            }
        }
        match (self.web.as_mut(), &self.error) {
            (Some(web), _) => web.show(px),
            (None, Some(error)) => {
                ui.centered_and_justified(|ui| ui.colored_label(Color32::LIGHT_RED, error.as_str()));
            }
            _ => {}
        }
    }
}

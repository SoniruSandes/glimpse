//! The preview window shell: our own title bar (pin, minimise, maximise, close), resize handles,
//! status bar, close behaviour. What goes inside lives in content.rs.
//!
//! The window is always undecorated. Pinning only changes behaviour (not always-on-top, stays open,
//! resizable); it never switches native window decorations at runtime.

use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use eframe::egui::{self, vec2, Align2, Color32, CursorIcon, FontId, Margin, RichText, Sense, Stroke};

use crate::{
    appinfo,
    content::{self, Pane},
    icons,
    media::WindowHint,
    theme, window,
};

pub struct Options {
    pub path: PathBuf,
    /// Settings: start pinned (stays open when you click away; Space / Esc / X close it).
    pub always_pin: bool,
    /// The file was reached through a .lnk shortcut: show where it really is.
    pub via_shortcut: bool,
}

pub fn run(mut options: Options) {
    window::set_sticky(options.always_pin);

    // A .lnk opened directly previews what it points to.
    let is_lnk = options.path.extension().map(|e| e.eq_ignore_ascii_case("lnk")).unwrap_or(false);
    if is_lnk {
        if let Some(target) = crate::shortcut::target(&options.path).filter(|t| t.exists()) {
            options.path = target;
            options.via_shortcut = true;
        }
    }
    appinfo::set_via_shortcut(options.via_shortcut);

    // A program gets the small info card (650 x 300); everything else the big window.
    let is_app = options
        .path
        .extension()
        .map(|e| appinfo::APP_EXTS.contains(&e.to_string_lossy().to_ascii_lowercase().as_str()))
        .unwrap_or(false);
    let (size, min_size) = if is_app { (appinfo::WINDOW_SIZE, [420.0, 240.0]) } else { ([1100.0, 720.0], [520.0, 360.0]) };

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size(size)
        .with_min_inner_size(min_size)
        .with_decorations(false)
        .with_title("Quick Look");
    // icons/window.png: the icon of the running window (taskbar button, Alt+Tab).
    if let Some(icon) = window_icon() {
        viewport = viewport.with_icon(icon);
    }
    if !options.always_pin {
        viewport = viewport.with_always_on_top();
    }
    // depth_buffer: the 3D model preview needs one (egui itself doesn't).
    let native = eframe::NativeOptions { viewport, centered: true, depth_buffer: 24, ..Default::default() };
    window::start_watchdog();
    let _ = eframe::run_native(
        "Quick Look",
        native,
        Box::new(move |cc| Ok(Box::new(PreviewApp::new(cc, options)))),
    );
    // Don't rely on normal teardown (worker threads, GPU context, libmpv): end the process now.
    window::fast_exit();
}

/// What the title bar and status bar currently show.
struct Info {
    title: String,
    type_label: String,
    size_text: String,
}

struct PreviewApp {
    info: Info,
    real_path: Option<String>,
    always_pin: bool,
    pane: Pane,
    pinned: bool,
    had_focus: bool,
    asked_focus: bool,
    /// A file dialog (3D texture picker) was open: don't close on lost focus while it is, or just after.
    was_modal: bool,
    modal_until: Option<Instant>,
    applied_hint: Option<(u8, i32, i32)>,
    #[cfg(windows)]
    corners_done: bool,
}

fn type_label_of(name: &str) -> String {
    std::path::Path::new(name)
        .extension()
        .map(|e| e.to_string_lossy().to_uppercase())
        .filter(|e| !e.is_empty())
        .unwrap_or_else(|| "FILE".to_string())
}

impl PreviewApp {
    fn new(cc: &eframe::CreationContext<'_>, options: Options) -> Self {
        theme::install(&cc.egui_ctx);
        // Ctrl +/- would scale the whole UI; we use those keys for image zoom instead.
        cc.egui_ctx.options_mut(|o| o.zoom_with_keyboard = false);

        let path = options.path;
        let title = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        let (type_label, size_text) = match std::fs::metadata(&path) {
            Ok(m) if m.is_dir() => ("FOLDER".to_string(), String::new()),
            Ok(m) => (type_label_of(&title), content::human_size(m.len())),
            Err(_) => ("FILE".to_string(), String::new()),
        };
        let real_path = options.via_shortcut.then(|| path.display().to_string());

        if options.always_pin {
            window::set_pinned(true, false);
        }

        let ctx = cc.egui_ctx.clone();
        let pane = Pane::spawn(&cc.egui_ctx, move || content::load_top(&path, &ctx));

        Self {
            info: Info { title, type_label, size_text },
            real_path,
            always_pin: options.always_pin,
            pane,
            pinned: options.always_pin,
            had_focus: false,
            asked_focus: false,
            was_modal: false,
            modal_until: None,
            applied_hint: None,
            #[cfg(windows)]
            corners_done: false,
        }
    }

    /// Pinned: not always-on-top, stays open when you click away, resizable.
    fn toggle_pin(&mut self, ctx: &egui::Context) {
        self.pinned = !self.pinned;
        // A hand-pinned window is left alone when the next preview opens; an "always pin" one is replaced.
        window::set_pinned(self.pinned, !self.always_pin);
        ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(if self.pinned {
            egui::WindowLevel::Normal
        } else {
            egui::WindowLevel::AlwaysOnTop
        }));
        ctx.send_viewport_cmd(egui::ViewportCommand::Title(if self.pinned {
            self.info.title.clone()
        } else {
            "Quick Look".to_string()
        }));
    }

    /// Audio gets a compact player window; video gets a window with the picture's shape (no black bars).
    fn apply_window_hint(&mut self, ctx: &egui::Context, maximized: bool) {
        let Some(hint) = self.pane.content.window_hint() else { return };
        let key = match hint {
            WindowHint::Audio => (0u8, 0, 0),
            WindowHint::Video { w, h } => (1u8, w as i32, h as i32),
        };
        if self.applied_hint == Some(key) || maximized {
            return;
        }
        self.applied_hint = Some(key);

        let monitor = ctx.input(|i| i.viewport().monitor_size).unwrap_or(vec2(1920.0, 1080.0));
        let size = match hint {
            WindowHint::Audio => vec2(630.0, 380.0),
            WindowHint::Video { w, h } => {
                // The window around the picture: 10 px margins, title bar, status bar.
                let chrome = vec2(20.0, 44.0 + 38.0 + 20.0);
                let room = vec2(monitor.x * 0.85 - chrome.x, monitor.y * 0.85 - chrome.y);
                let fit = (room.x / w).min(room.y / h);
                let scale = fit.min(1.0).max((640.0 / w).min(fit)); // up to native size, at least ~640 wide
                vec2(w * scale + chrome.x, h * scale + chrome.y)
            }
        };
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
        ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(
            ((monitor.x - size.x) / 2.0).max(0.0),
            ((monitor.y - size.y) / 2.0).max(0.0),
        )));
    }

    fn title_bar(&mut self, ctx: &egui::Context, maximized: bool) {
        egui::TopBottomPanel::top("header")
            .exact_height(44.0)
            .frame(egui::Frame::none().fill(theme::BAR_FILL).inner_margin(Margin::symmetric(10.0, 5.0)))
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    let buttons = if self.pinned { 4.0 } else { 2.0 };
                    let drag_width = (ui.available_width() - buttons * 36.0 - 6.0).max(10.0);
                    let (rect, response) = ui.allocate_exact_size(vec2(drag_width, 32.0), Sense::click_and_drag());
                    ui.painter().text(
                        rect.left_center() + vec2(6.0, 0.0),
                        Align2::LEFT_CENTER,
                        theme::file_icon(&self.info.title),
                        FontId::new(20.0, theme::icon_family()),
                        Color32::from_gray(185),
                    );
                    ui.painter().with_clip_rect(rect).text(
                        rect.left_center() + vec2(36.0, 0.0),
                        Align2::LEFT_CENTER,
                        &self.info.title,
                        FontId::proportional(15.0),
                        Color32::from_gray(235),
                    );
                    if response.drag_started() && !maximized {
                        ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                    }
                    if response.double_clicked() && self.pinned {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
                    }

                    let pin_tip = if self.pinned { "Unpin (P)" } else { "Pin window (P)" };
                    if theme::icon_button(ui, icons::PUSH_PIN, pin_tip, self.pinned, true).clicked() {
                        self.toggle_pin(ctx);
                    }
                    if self.pinned {
                        if theme::icon_button(ui, icons::MINUS, "Minimise", false, true).clicked() {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                        }
                        let (glyph, tip) = if maximized {
                            (icons::COPY_SIMPLE, "Restore")
                        } else {
                            (icons::SQUARE, "Maximise")
                        };
                        if theme::icon_button(ui, glyph, tip, false, true).clicked() {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
                        }
                    }
                    if theme::icon_button(ui, icons::X, "Close", false, true).clicked() {
                        window::fast_exit();
                    }
                });
            });
    }

    fn status_bar(&mut self, ctx: &egui::Context) {
        let chips = self.pane.content.chips();
        egui::TopBottomPanel::bottom("footer")
            .exact_height(38.0)
            .frame(egui::Frame::none().fill(theme::BAR_FILL).inner_margin(Margin::symmetric(16.0, 6.0)))
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    // Right end first: actions of the current content (e.g. the 3D "Textures" menu).
                    // Everything else fills the space that is left, on the left.
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    self.pane.content.footer_ui(ui);
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    ui.label(RichText::new(&self.info.type_label).size(13.5).strong().color(Color32::from_gray(230)));
                    for chip in &chips {
                        ui.add(egui::Separator::default().vertical().spacing(10.0));
                        ui.label(theme::icon_text(chip.icon, 16.0).color(Color32::from_gray(150)));
                        ui.label(RichText::new(&chip.text).size(13.5).color(Color32::from_gray(215)));
                    }
                    if !self.info.size_text.is_empty() {
                        ui.add(egui::Separator::default().vertical().spacing(10.0));
                        ui.label(theme::icon_text(icons::FLOPPY_DISK, 16.0).color(Color32::from_gray(150)));
                        ui.label(RichText::new(&self.info.size_text).size(13.5).color(Color32::from_gray(215)));
                    }
                    // Opened through a shortcut: show the real location.
                    if let Some(real) = &self.real_path {
                        ui.add(egui::Separator::default().vertical().spacing(10.0));
                        ui.label(theme::icon_text(icons::LINK_SIMPLE, 16.0).color(Color32::from_gray(150)));
                        ui.add(
                            egui::Label::new(RichText::new(real).size(13.5).color(Color32::from_gray(215)))
                                .wrap_mode(egui::TextWrapMode::Truncate),
                        )
                        .on_hover_text(real);
                    }
                    });
                    });
                });
            });
    }
}

impl eframe::App for PreviewApp {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        // Windows: the web view is a child window of this window; the watchdog watches it too.
        #[cfg(windows)]
        {
            use raw_window_handle::{HasWindowHandle, RawWindowHandle};
            if let Ok(handle) = frame.window_handle() {
                if let RawWindowHandle::Win32(h) = handle.as_raw() {
                    window::set_window(h.hwnd.get());
                    if !self.corners_done {
                        window::round_corners(h.hwnd.get());
                        self.corners_done = true;
                    }
                }
            }
        }
        #[cfg(not(windows))]
        let _ = frame;

        if ctx.input(|i| i.viewport().close_requested()) {
            window::fast_exit();
        }

        if !self.asked_focus {
            self.asked_focus = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }

        let typing = ctx.wants_keyboard_input(); // e.g. the PDF password box
        if !typing && ctx.input(|i| i.key_pressed(egui::Key::P)) {
            self.toggle_pin(ctx);
        }

        // Clicking away closes an unpinned preview. We poll rather than wait for a focus event:
        // a child window (the web view) can take focus without the window ever reporting that it lost it.
        let focus_state = own_focus_state(ctx);
        if focus_state == Some(true) {
            self.had_focus = true;
        }
        // A native file dialog (texture picker) takes the focus: that is not "clicking away". While it is open the
        // window also steps down from always-on-top so the dialog can't hide behind it, and afterwards it takes
        // the focus back.
        let modal = self.pane.content.modal_busy();
        if modal {
            self.modal_until = Some(Instant::now() + Duration::from_millis(1500));
        }
        if modal != self.was_modal {
            self.was_modal = modal;
            if !self.pinned {
                ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(if modal {
                    egui::WindowLevel::Normal
                } else {
                    egui::WindowLevel::AlwaysOnTop
                }));
            }
            if !modal {
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            }
        }
        let modal_grace = modal || self.modal_until.map_or(false, |t| Instant::now() < t);
        let lost_focus = !self.pinned && !modal_grace && self.had_focus && focus_state == Some(false);
        // Esc / Space close an unpinned preview, and also an "always pin" one (like Quick Look).
        // Not while typing: Esc first leaves a text box (password, find) and only then closes the window.
        let close_key = (!self.pinned || self.always_pin)
            && !typing
            && ctx.input(|i| i.key_pressed(egui::Key::Escape) || i.key_pressed(egui::Key::Space));
        if lost_focus || close_key {
            window::fast_exit();
        }
        if !self.pinned {
            ctx.request_repaint_after(Duration::from_millis(150));
        }

        // Browsing images: the title and status bar follow the current image.
        if let Some((name, size)) = self.pane.content.title_override() {
            if self.info.title != name {
                self.info.type_label = type_label_of(&name);
                self.info.title = name;
            }
            self.info.size_text = content::human_size(size);
        }

        let maximized = ctx.input(|i| i.viewport().maximized).unwrap_or(false);
        self.apply_window_hint(ctx, maximized);
        self.title_bar(ctx, maximized);
        if !self.pane.content.hides_status_bar() {
            self.status_bar(ctx);
        }
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(theme::PANEL_FILL).inner_margin(Margin::same(10.0)))
            .show(ctx, |ui| self.pane.ui(ui));

        // Thin window outline, and (pinned) invisible resize handles around the edges.
        ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("window_border")))
            .rect_stroke(ctx.screen_rect().shrink(0.5), 0.0, Stroke::new(1.0, Color32::from_white_alpha(38)));
        if self.pinned && !maximized {
            resize_handles(ctx);
        }
    }
}

fn resize_handles(ctx: &egui::Context) {
    use egui::ResizeDirection as D;
    let r = ctx.screen_rect();
    let (e, c) = (6.0, 14.0);
    // Edges first, corners last so the corners win where they overlap.
    let handles = [
        ("rs_n", r.left_top() + vec2(c, 0.0), vec2(r.width() - 2.0 * c, e), CursorIcon::ResizeVertical, D::North),
        ("rs_s", r.left_bottom() + vec2(c, -e), vec2(r.width() - 2.0 * c, e), CursorIcon::ResizeVertical, D::South),
        ("rs_w", r.left_top() + vec2(0.0, c), vec2(e, r.height() - 2.0 * c), CursorIcon::ResizeHorizontal, D::West),
        ("rs_e", r.right_top() + vec2(-e, c), vec2(e, r.height() - 2.0 * c), CursorIcon::ResizeHorizontal, D::East),
        ("rs_nw", r.left_top(), vec2(c, c), CursorIcon::ResizeNwSe, D::NorthWest),
        ("rs_ne", r.right_top() + vec2(-c, 0.0), vec2(c, c), CursorIcon::ResizeNeSw, D::NorthEast),
        ("rs_sw", r.left_bottom() + vec2(0.0, -c), vec2(c, c), CursorIcon::ResizeNeSw, D::SouthWest),
        ("rs_se", r.right_bottom() + vec2(-c, -c), vec2(c, c), CursorIcon::ResizeNwSe, D::SouthEast),
    ];
    for (id, pos, size, cursor, direction) in handles {
        egui::Area::new(egui::Id::new(id))
            .order(egui::Order::Foreground)
            .fixed_pos(pos)
            .show(ctx, |ui| {
                let (_, response) = ui.allocate_exact_size(size, Sense::drag());
                let response = response.on_hover_cursor(cursor);
                if response.drag_started_by(egui::PointerButton::Primary) {
                    ctx.send_viewport_cmd(egui::ViewportCommand::BeginResize(direction));
                }
            });
    }
}

/// Does this app own the foreground window? (None: unknown.)
#[cfg(windows)]
fn own_focus_state(_ctx: &egui::Context) -> Option<bool> {
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};
    unsafe {
        let foreground = GetForegroundWindow();
        if foreground.0.is_null() {
            return None;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(foreground, Some(&mut pid));
        Some(pid == std::process::id())
    }
}

#[cfg(not(windows))]
fn own_focus_state(ctx: &egui::Context) -> Option<bool> {
    ctx.input(|i| i.viewport().focused)
}

/// icons/window.png -> the window icon.
fn window_icon() -> Option<egui::IconData> {
    let bytes = include_bytes!("../icons/window.png");
    let image = image::load_from_memory_with_format(bytes, image::ImageFormat::Png).ok()?.to_rgba8();
    let (width, height) = image.dimensions();
    Some(egui::IconData { rgba: image.into_raw(), width, height })
}

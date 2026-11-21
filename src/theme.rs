//! Shared look: theme, icon font, icon buttons, list rows, the floating page switcher.

use eframe::egui::{
    self, pos2, vec2, Align2, Color32, FontFamily, FontId, Margin, Rect, Response, RichText, Rounding, Sense, Stroke,
};

use crate::icons;

pub const ACCENT: Color32 = Color32::from_rgb(86, 140, 255);
pub const PANEL_FILL: Color32 = Color32::from_rgb(24, 24, 28);
pub const BAR_FILL: Color32 = Color32::from_rgb(30, 30, 35);
pub const CARD_FILL: Color32 = Color32::from_rgb(35, 35, 41);

pub fn icon_family() -> FontFamily {
    FontFamily::Name("icons".into())
}

pub fn icon_text(glyph: &str, size: f32) -> RichText {
    RichText::new(glyph).family(icon_family()).size(size)
}

pub fn install(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "phosphor".to_owned(),
        egui::FontData::from_static(include_bytes!("../assets/Phosphor.ttf")),
    );
    fonts.families.insert(icon_family(), vec!["phosphor".to_owned()]);
    ctx.set_fonts(fonts);

    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = PANEL_FILL;
    visuals.window_fill = PANEL_FILL;
    visuals.extreme_bg_color = Color32::from_rgb(17, 17, 20);
    visuals.faint_bg_color = Color32::from_rgb(31, 31, 36);
    visuals.selection.bg_fill = ACCENT.gamma_multiply(0.55);
    let round = Rounding::same(8.0);
    visuals.widgets.noninteractive.rounding = round;
    visuals.widgets.inactive.rounding = round;
    visuals.widgets.hovered.rounding = round;
    visuals.widgets.active.rounding = round;
    visuals.widgets.open.rounding = round;
    visuals.widgets.inactive.bg_fill = Color32::from_rgb(46, 46, 55);
    visuals.widgets.inactive.weak_bg_fill = Color32::from_rgb(46, 46, 55);
    visuals.widgets.hovered.bg_fill = Color32::from_rgb(60, 60, 72);
    visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(60, 60, 72);
    ctx.set_visuals(visuals);

    ctx.style_mut(|style| {
        style.spacing.item_spacing = vec2(8.0, 6.0);
        style.spacing.button_padding = vec2(12.0, 6.0);
        style.text_styles.insert(egui::TextStyle::Body, FontId::proportional(14.5));
        style.text_styles.insert(egui::TextStyle::Button, FontId::proportional(14.5));
        style.text_styles.insert(egui::TextStyle::Small, FontId::proportional(12.5));
        style.text_styles.insert(egui::TextStyle::Heading, FontId::proportional(22.0));
        style.text_styles.insert(egui::TextStyle::Monospace, FontId::monospace(13.5));
    });
}

/// A flat icon button with a hover/active background. `active` shows it as toggled on.
pub fn icon_button(ui: &mut egui::Ui, glyph: &str, tooltip: &str, active: bool, enabled: bool) -> Response {
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, response) = ui.allocate_exact_size(vec2(34.0, 30.0), sense);
    if ui.is_rect_visible(rect) {
        let hovered = enabled && response.hovered();
        let pressed = enabled && response.is_pointer_button_down_on();
        let fill = if pressed {
            Color32::from_white_alpha(40)
        } else if active {
            ACCENT.gamma_multiply(0.38)
        } else if hovered {
            Color32::from_white_alpha(24)
        } else {
            Color32::TRANSPARENT
        };
        ui.painter().rect_filled(rect, 7.0, fill);
        let color = if !enabled {
            Color32::from_gray(85)
        } else if active {
            Color32::WHITE
        } else if hovered {
            Color32::WHITE
        } else {
            Color32::from_gray(205)
        };
        ui.painter()
            .text(rect.center(), Align2::CENTER_CENTER, glyph, FontId::new(18.0, icon_family()), color);
    }
    if tooltip.is_empty() {
        response
    } else {
        response.on_hover_text(tooltip)
    }
}

pub const ROW_HEIGHT: f32 = 30.0;

/// One row of a file list: icon, name (clipped), size on the right.
pub fn list_row(ui: &mut egui::Ui, selected: bool, glyph: &str, name: &str, trailing: &str, dim: bool) -> Response {
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_HEIGHT), Sense::click());
    if ui.is_rect_visible(rect) {
        if selected {
            ui.painter().rect_filled(rect, 7.0, ACCENT.gamma_multiply(0.32));
        } else if response.hovered() {
            ui.painter().rect_filled(rect, 7.0, Color32::from_white_alpha(14));
        }
        let text_color = if dim { Color32::from_gray(170) } else { Color32::from_gray(230) };
        let icon_color = if selected { Color32::WHITE } else { Color32::from_gray(160) };
        ui.painter().text(
            pos2(rect.left() + 18.0, rect.center().y),
            Align2::CENTER_CENTER,
            glyph,
            FontId::new(17.0, icon_family()),
            icon_color,
        );
        let trailing_width = if trailing.is_empty() { 0.0 } else { 74.0 };
        let clip = Rect::from_min_max(
            pos2(rect.left() + 34.0, rect.top()),
            pos2(rect.right() - trailing_width - 6.0, rect.bottom()),
        );
        ui.painter().with_clip_rect(clip).text(
            pos2(rect.left() + 34.0, rect.center().y),
            Align2::LEFT_CENTER,
            name,
            FontId::proportional(14.5),
            text_color,
        );
        if !trailing.is_empty() {
            ui.painter().text(
                pos2(rect.right() - 8.0, rect.center().y),
                Align2::RIGHT_CENTER,
                trailing,
                FontId::proportional(12.5),
                Color32::from_gray(140),
            );
        }
    }
    response
}

/// A pill centred at the bottom of `area`: previous, "3 / 12", next. Returns (previous clicked, next clicked).
pub fn floating_pager(
    ctx: &egui::Context,
    id: egui::Id,
    area: Rect,
    label: &str,
    can_prev: bool,
    can_next: bool,
) -> (bool, bool) {
    let mut prev = false;
    let mut next = false;
    egui::Area::new(id)
        .order(egui::Order::Foreground)
        .pivot(Align2::CENTER_BOTTOM)
        .fixed_pos(pos2(area.center().x, area.bottom() - 16.0))
        .show(ctx, |ui| {
            egui::Frame::none()
                .fill(Color32::from_rgba_unmultiplied(36, 36, 43, 238))
                .rounding(Rounding::same(22.0))
                .stroke(Stroke::new(1.0, Color32::from_white_alpha(30)))
                .inner_margin(Margin::symmetric(6.0, 4.0))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 2.0;
                        prev = icon_button(ui, icons::CARET_LEFT, "Previous", false, can_prev).clicked();
                        ui.add_sized([86.0, 30.0], egui::Label::new(RichText::new(label).size(14.5).strong()));
                        next = icon_button(ui, icons::CARET_RIGHT, "Next", false, can_next).clicked();
                    });
                });
        });
    (prev, next)
}

/// Icon for a file, by extension.
pub fn file_icon(name: &str) -> &'static str {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico" | "tif" | "tiff" | "psd" | "ai" | "svg" => icons::FILE_IMAGE,
        "mp4" | "mkv" | "avi" | "mov" | "wmv" | "webm" | "flv" | "m4v" | "mpg" | "mpeg" | "ts" | "m2ts" | "3gp" | "ogv" => {
            icons::FILE_VIDEO
        }
        "mp3" | "wav" | "flac" | "ogg" | "oga" | "opus" | "m4a" | "aac" | "wma" | "aiff" | "ape" | "mka" => icons::FILE_AUDIO,
        "exe" | "msi" | "dll" => icons::FILE_CODE,
        "lnk" => icons::LINK_SIMPLE,
        "aep" | "aepx" => icons::FILM_STRIP,
        "obj" | "stl" | "ply" | "gltf" | "glb" => icons::CUBE,
        "pdf" => icons::FILE_PDF,
        "zip" | "7z" | "rar" | "jar" | "apk" | "cbz" | "cbr" | "war" => icons::FILE_ZIP,
        "doc" | "docx" | "docm" => icons::FILE_DOC,
        "xls" | "xlsx" | "xlsm" | "xlsb" | "ods" => icons::FILE_XLS,
        "csv" | "tsv" => icons::FILE_CSV,
        "ppt" | "pptx" | "pptm" => icons::FILE_PPT,
        "html" | "htm" | "xhtml" => icons::FILE_HTML,
        "js" => icons::FILE_JS,
        "css" => icons::FILE_CSS,
        "tsx" => icons::FILE_TSX,
        "jsx" => icons::FILE_JSX,
        "py" => icons::FILE_PY,
        "rs" => icons::FILE_RS,
        "c" | "h" => icons::FILE_C,
        "cpp" | "hpp" | "cc" => icons::FILE_CPP,
        "sql" => icons::FILE_SQL,
        "md" => icons::FILE_MD,
        "txt" | "log" => icons::FILE_TXT,
        "vue" => icons::FILE_VUE,
        "json" | "xml" | "toml" | "yaml" | "yml" | "java" | "go" | "cs" | "sh" | "bat" | "ps1" => icons::FILE_CODE,
        _ => icons::FILE,
    }
}

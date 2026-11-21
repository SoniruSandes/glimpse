//! Two "summary" views that don't show the file's pixels but what's inside it:
//!   * `AppView`  - a Windows program: icon, name, location, version, size (650 x 300 card)
//!   * `AepView`  - an After Effects project: version, compositions, footage

use eframe::egui::{self, pos2, vec2, Align2, Color32, FontId, Margin, Rect, RichText, Rounding, Sense};

use crate::{
    aep::{self, AepInfo},
    appinfo::AppInfo,
    content::Chip,
    icons, theme,
};

const LABEL: Color32 = Color32::from_gray(150);
const VALUE: Color32 = Color32::from_gray(228);

/// 5.44 MB (two decimals: the card is about precise numbers).
fn size_text(bytes: u64) -> String {
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
        format!("{value:.2} {}", UNITS[unit])
    }
}

// ------------------------------------------------------------------------------------------
// Programs
// ------------------------------------------------------------------------------------------

pub struct AppView {
    info: AppInfo,
    icon: Option<egui::TextureHandle>,
}

impl AppView {
    pub fn new(ctx: &egui::Context, info: AppInfo) -> Self {
        let icon = info
            .icon
            .clone()
            .map(|image| ctx.load_texture("app-icon", image, egui::TextureOptions::LINEAR));
        Self { info, icon }
    }

    pub fn chips(&self) -> Vec<Chip> {
        let mut chips = Vec::new();
        if let Some(arch) = self.info.arch {
            chips.push(Chip { icon: icons::CUBE, text: arch.to_string() });
        }
        chips
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        let area = ui.available_rect_before_wrap();
        let icon_box = Rect::from_center_size(pos2(area.left() + 68.0, area.center().y), vec2(112.0, 112.0));
        match &self.icon {
            Some(tex) => {
                let size = tex.size_vec2();
                let fit = (104.0 / size.x).min(104.0 / size.y);
                ui.painter().image(
                    tex.id(),
                    Rect::from_center_size(icon_box.center(), size * fit),
                    Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                    Color32::WHITE,
                );
            }
            None => {
                // A program without an icon of its own.
                ui.painter().circle_filled(icon_box.center(), 52.0, theme::CARD_FILL);
                ui.painter().text(
                    icon_box.center(),
                    Align2::CENTER_CENTER,
                    icons::CUBE,
                    FontId::new(52.0, theme::icon_family()),
                    Color32::from_gray(150),
                );
            }
        }

        let text_area = Rect::from_min_max(pos2(area.left() + 148.0, area.top()), area.max);
        let mut rows: Vec<(&str, String, Option<&str>)> = Vec::new();
        let location = (self.info.location.clone(), self.info.via_shortcut.then_some(icons::LINK_SIMPLE));
        rows.push(("Location", location.0, location.1));
        rows.push(("Total Size", size_text(self.info.size), None));
        if let Some(v) = &self.info.file_version {
            rows.push(("File Version", v.clone(), None));
        }
        if let Some(v) = &self.info.product_version {
            rows.push(("Product Version", v.clone(), None));
        }
        if let Some(v) = &self.info.company {
            rows.push(("Company", v.clone(), None));
        }
        let subtitle = self
            .info
            .description
            .clone()
            .or_else(|| self.info.product_name.clone())
            .filter(|s| !s.eq_ignore_ascii_case(&self.info.file_name));

        let content_h = 34.0 + if subtitle.is_some() { 22.0 } else { 0.0 } + 12.0 + rows.len() as f32 * 27.0;
        ui.allocate_new_ui(egui::UiBuilder::new().max_rect(text_area), |ui| {
            ui.add_space(((text_area.height() - content_h) / 2.0).max(0.0));
            ui.spacing_mut().item_spacing.y = 0.0;

            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                ui.add(
                    egui::Label::new(RichText::new(&self.info.file_name).size(22.0).strong().color(Color32::WHITE))
                        .truncate(),
                )
                .on_hover_text(&self.info.file_name);
                if let Some(arch) = self.info.arch {
                    egui::Frame::none()
                        .fill(Color32::from_rgb(0, 103, 192))
                        .rounding(Rounding::same(4.0))
                        .inner_margin(Margin::symmetric(6.0, 1.0))
                        .show(ui, |ui| {
                            ui.label(RichText::new(arch).size(12.5).strong().color(Color32::WHITE));
                        });
                }
            });
            if let Some(sub) = &subtitle {
                ui.add_space(2.0);
                ui.add(egui::Label::new(RichText::new(sub).size(14.0).color(LABEL)).truncate());
            }
            ui.add_space(12.0);

            for (label, value, glyph) in &rows {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    let (rect, _) = ui.allocate_exact_size(vec2(126.0, 27.0), Sense::hover());
                    ui.painter().text(
                        rect.left_center(),
                        Align2::LEFT_CENTER,
                        label,
                        FontId::proportional(14.5),
                        LABEL,
                    );
                    if let Some(glyph) = glyph {
                        ui.label(theme::icon_text(glyph, 15.0).color(theme::ACCENT));
                        ui.add_space(5.0);
                    }
                    ui.add(egui::Label::new(RichText::new(value).size(14.5).color(VALUE)).truncate())
                        .on_hover_text(value);
                });
            }
        });
    }
}

// ------------------------------------------------------------------------------------------
// After Effects projects
// ------------------------------------------------------------------------------------------

enum Row {
    Heading(String),
    Comp(usize),
    Footage(usize),
    Note(String),
}

pub struct AepView {
    info: AepInfo,
    rows: Vec<Row>,
}

const AEP_ROW: f32 = 28.0;

impl AepView {
    pub fn new(info: AepInfo) -> Self {
        let mut rows = Vec::new();
        if !info.comps.is_empty() {
            rows.push(Row::Heading(format!("Compositions ({})", info.comps.len())));
            rows.extend((0..info.comps.len()).map(Row::Comp));
        }
        if !info.footage.is_empty() {
            rows.push(Row::Heading(format!("Footage ({})", info.footage_count)));
            rows.extend((0..info.footage.len()).map(Row::Footage));
            let unnamed = info.footage_count.saturating_sub(info.footage.len());
            if unnamed > 0 {
                rows.push(Row::Note(format!("+ {unnamed} more footage items without a stored name")));
            }
        }
        if info.truncated {
            rows.push(Row::Note("The project is very large; only the first part is listed".into()));
        }
        Self { info, rows }
    }

    pub fn chips(&self) -> Vec<Chip> {
        let mut chips = Vec::new();
        if let Some(v) = self.info.version {
            chips.push(Chip { icon: icons::FILM_STRIP, text: format!("AE {}.{}", v.major, v.minor) });
        }
        chips.push(Chip { icon: icons::STACK, text: format!("{} comps", self.info.comps.len()) });
        chips
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        // Header: which After Effects wrote it, and how much is inside.
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 12.0;
            ui.label(theme::icon_text(icons::FILM_STRIP, 34.0).color(Color32::from_rgb(154, 153, 255)));
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                let title = self
                    .info
                    .version
                    .map(|v| v.label())
                    .unwrap_or_else(|| "After Effects project".to_string());
                ui.label(RichText::new(title).size(18.0).strong().color(Color32::WHITE));
                ui.label(
                    RichText::new(format!(
                        "{} compositions  ·  {} footage items  ·  {} folders",
                        self.info.comps.len(),
                        self.info.footage_count,
                        self.info.folders
                    ))
                    .size(13.5)
                    .color(LABEL),
                );
            });
        });
        ui.add_space(8.0);
        ui.separator();

        if self.rows.is_empty() {
            ui.centered_and_justified(|ui| ui.weak("This project has no compositions or named footage"));
            return;
        }
        let info = &self.info;
        let rows = &self.rows;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show_rows(ui, AEP_ROW, rows.len(), |ui, range| {
                for row in &rows[range] {
                    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), AEP_ROW), Sense::hover());
                    let painter = ui.painter().with_clip_rect(rect);
                    match row {
                        Row::Heading(text) => {
                            painter.text(
                                rect.left_center() + vec2(2.0, 4.0),
                                Align2::LEFT_CENTER,
                                text.to_uppercase(),
                                FontId::proportional(12.5),
                                theme::ACCENT,
                            );
                        }
                        Row::Note(text) => {
                            painter.text(rect.left_center() + vec2(8.0, 0.0), Align2::LEFT_CENTER, text, FontId::proportional(13.0), LABEL);
                        }
                        Row::Comp(i) => {
                            let c = &info.comps[*i];
                            if response.hovered() {
                                painter.rect_filled(rect, 6.0, Color32::from_white_alpha(12));
                            }
                            painter.text(
                                pos2(rect.left() + 16.0, rect.center().y),
                                Align2::CENTER_CENTER,
                                icons::FILM_STRIP,
                                FontId::new(16.0, theme::icon_family()),
                                Color32::from_gray(150),
                            );
                            let detail = format!(
                                "{} × {}  ·  {} fps  ·  {}",
                                c.width,
                                c.height,
                                trim_fps(c.fps),
                                aep::duration_text(c.duration)
                            );
                            let detail_galley = painter.layout_no_wrap(detail, FontId::proportional(13.0), LABEL);
                            let detail_w = detail_galley.size().x;
                            painter.galley(
                                pos2(rect.right() - detail_w - 8.0, rect.center().y - detail_galley.size().y / 2.0),
                                detail_galley,
                                LABEL,
                            );
                            painter
                                .with_clip_rect(Rect::from_min_max(
                                    pos2(rect.left() + 32.0, rect.top()),
                                    pos2(rect.right() - detail_w - 20.0, rect.bottom()),
                                ))
                                .text(
                                    pos2(rect.left() + 32.0, rect.center().y),
                                    Align2::LEFT_CENTER,
                                    &c.name,
                                    FontId::proportional(14.5),
                                    VALUE,
                                );
                        }
                        Row::Footage(i) => {
                            let f = &info.footage[*i];
                            if response.hovered() {
                                painter.rect_filled(rect, 6.0, Color32::from_white_alpha(12));
                            }
                            painter.text(
                                pos2(rect.left() + 16.0, rect.center().y),
                                Align2::CENTER_CENTER,
                                icons::FILE,
                                FontId::new(16.0, theme::icon_family()),
                                Color32::from_gray(150),
                            );
                            let text = match &f.file {
                                Some(file) if *file != f.name => format!("{}   ({file})", f.name),
                                _ => f.name.clone(),
                            };
                            painter.text(
                                pos2(rect.left() + 32.0, rect.center().y),
                                Align2::LEFT_CENTER,
                                text,
                                FontId::proportional(14.5),
                                VALUE,
                            );
                        }
                    }
                }
            });
    }
}

/// 29.97, 24, 23.976 -> "29.97", "24", "23.976"
fn trim_fps(fps: f64) -> String {
    let s = format!("{fps:.3}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

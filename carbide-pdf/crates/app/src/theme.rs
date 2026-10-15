//! Dark theme: charcoal surfaces, grey tab strips, white text, restrained accents.

use eframe::egui::{self, Color32, FontId, TextStyle};

pub const TOP_BAR: Color32 = Color32::from_rgb(38, 38, 38);
pub const TAB_STRIP: Color32 = Color32::from_rgb(86, 86, 86);
pub const TAB_ACTIVE: Color32 = Color32::from_rgb(122, 122, 122);
pub const RIBBON: Color32 = Color32::from_rgb(46, 46, 46);
pub const CONTENT: Color32 = Color32::from_rgb(28, 28, 28);
pub const SIDEBAR: Color32 = Color32::from_rgb(42, 42, 42);
pub const SIDEBAR_SELECTED: Color32 = Color32::from_rgb(112, 112, 112);
pub const BOTTOM_BAR: Color32 = Color32::from_rgb(88, 88, 88);
pub const TEXT: Color32 = Color32::from_rgb(236, 236, 236);
pub const TEXT_DIM: Color32 = Color32::from_rgb(160, 160, 160);
pub const HAIRLINE: Color32 = Color32::from_rgb(70, 70, 70);
pub const ACCENT: Color32 = Color32::from_rgb(92, 150, 235);
pub const NOTICE: Color32 = Color32::from_rgb(52, 62, 86);

pub fn apply(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = CONTENT;
    visuals.window_fill = Color32::from_rgb(44, 44, 44);
    visuals.extreme_bg_color = Color32::from_rgb(22, 22, 22);
    visuals.faint_bg_color = Color32::from_rgb(36, 36, 36);
    visuals.override_text_color = Some(TEXT);
    visuals.widgets.noninteractive.bg_fill = Color32::from_rgb(40, 40, 40);
    visuals.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, HAIRLINE);
    visuals.widgets.inactive.bg_fill = Color32::from_rgb(62, 62, 62);
    visuals.widgets.inactive.weak_bg_fill = Color32::from_rgb(58, 58, 58);
    visuals.widgets.hovered.bg_fill = Color32::from_rgb(84, 84, 84);
    visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(76, 76, 76);
    visuals.widgets.active.bg_fill = Color32::from_rgb(104, 104, 104);
    visuals.widgets.active.weak_bg_fill = Color32::from_rgb(96, 96, 96);
    visuals.selection.bg_fill = Color32::from_rgb(72, 96, 140);
    visuals.selection.stroke = egui::Stroke::new(1.0, Color32::WHITE);
    visuals.window_stroke = egui::Stroke::new(1.0, HAIRLINE);
    // Both theme slots get the same look, so a system light/dark switch does not change it.
    ctx.all_styles_mut(move |style| {
        style.visuals = visuals.clone();
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        style.spacing.button_padding = egui::vec2(10.0, 4.0);
        style
            .text_styles
            .insert(TextStyle::Body, FontId::proportional(13.5));
        style
            .text_styles
            .insert(TextStyle::Button, FontId::proportional(13.5));
        style
            .text_styles
            .insert(TextStyle::Small, FontId::proportional(11.0));
        style
            .text_styles
            .insert(TextStyle::Heading, FontId::proportional(18.0));
    });
}

//! Visual identity: fonts, colours and the translucent "glass" panels.

use egui::epaint::text::VariationCoords;
use egui::{Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Frame, Margin, Shadow, Stroke, TextStyle};
use std::sync::Arc;

pub const ACCENT: Color32 = Color32::from_rgb(255, 181, 71);
pub const ACCENT_DIM: Color32 = Color32::from_rgb(170, 118, 40);
pub const TEXT: Color32 = Color32::from_rgb(242, 243, 245);
pub const MUTED: Color32 = Color32::from_rgb(160, 166, 178);
pub const BG: Color32 = Color32::from_rgb(12, 13, 16);
pub const PANEL: Color32 = Color32::from_rgb(20, 22, 27);
pub const DANGER: Color32 = Color32::from_rgb(240, 98, 92);
pub const OK: Color32 = Color32::from_rgb(98, 210, 140);

pub fn glass() -> Color32 {
    Color32::from_rgba_unmultiplied(14, 16, 20, 205)
}

pub fn glass_hover() -> Color32 {
    Color32::from_rgba_unmultiplied(40, 44, 52, 225)
}

pub fn hairline() -> Stroke {
    Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 255, 255, 22))
}

pub fn shadow() -> Shadow {
    Shadow { offset: [0, 6], blur: 24, spread: 0, color: Color32::from_black_alpha(90) }
}

/// The translucent rounded panel used for every overlay.
pub fn glass_frame() -> Frame {
    Frame::new().fill(glass()).stroke(hairline()).corner_radius(CornerRadius::same(16)).inner_margin(Margin::same(10)).shadow(shadow())
}

/// An inset card for read-outs inside side panels.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))] // only the native stitching workspace uses it so far
pub fn card_frame() -> Frame {
    Frame::new().fill(Color32::from_rgb(27, 30, 36)).stroke(hairline()).corner_radius(CornerRadius::same(10)).inner_margin(Margin::same(10))
}

pub fn heading_font(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("semibold".into()))
}

pub fn body_font(size: f32) -> FontId {
    FontId::new(size, FontFamily::Proportional)
}

pub fn install(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    let inter: &'static [u8] = include_bytes!("../assets/Inter.ttf");
    let mut regular = FontData::from_static(inter);
    regular.tweak.coords = VariationCoords::new([(b"wght", 430.0)]);
    let mut semibold = FontData::from_static(inter);
    semibold.tweak.coords = VariationCoords::new([(b"wght", 620.0)]);
    fonts.font_data.insert("inter".into(), Arc::new(regular));
    fonts.font_data.insert("inter-semibold".into(), Arc::new(semibold));
    fonts.families.get_mut(&FontFamily::Proportional).unwrap().insert(0, "inter".into());
    let mut semi = vec!["inter-semibold".to_string()];
    semi.extend(fonts.families[&FontFamily::Proportional].iter().skip(1).cloned());
    fonts.families.insert(FontFamily::Name("semibold".into()), semi);
    ctx.set_fonts(fonts);

    // Nodal is always dark (panoramas read best on dark chrome), whatever the OS prefers.
    ctx.set_theme(egui::ThemePreference::Dark);
    ctx.all_styles_mut(|style| {
        style.text_styles = [
            (TextStyle::Heading, heading_font(19.0)),
            (TextStyle::Body, body_font(14.0)),
            (TextStyle::Button, body_font(14.0)),
            (TextStyle::Small, body_font(11.5)),
            (TextStyle::Monospace, FontId::monospace(13.0)),
        ]
        .into();
        style.spacing.item_spacing = egui::vec2(8.0, 7.0);
        style.spacing.button_padding = egui::vec2(12.0, 6.0);
        style.spacing.interact_size.y = 30.0;
        style.spacing.slider_width = 170.0;
        style.spacing.combo_width = 170.0;
        style.animation_time = 0.14;
        let v = &mut style.visuals;
        *v = egui::Visuals::dark();
        v.panel_fill = PANEL;
        v.window_fill = PANEL;
        v.extreme_bg_color = Color32::from_rgb(10, 11, 14);
        v.faint_bg_color = Color32::from_rgb(28, 31, 37);
        v.override_text_color = Some(TEXT);
        v.hyperlink_color = ACCENT;
        v.selection.bg_fill = ACCENT_DIM;
        v.selection.stroke = Stroke::new(1.0, ACCENT);
        v.window_corner_radius = CornerRadius::same(16);
        v.menu_corner_radius = CornerRadius::same(12);
        v.window_stroke = hairline();
        v.window_shadow = shadow();
        v.popup_shadow = shadow();
        let r = CornerRadius::same(9);
        for w in
            [&mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active, &mut v.widgets.open, &mut v.widgets.noninteractive]
        {
            w.corner_radius = r;
        }
        v.widgets.noninteractive.bg_stroke = hairline();
        v.widgets.inactive.bg_fill = Color32::from_rgb(36, 39, 46);
        v.widgets.inactive.weak_bg_fill = Color32::from_rgb(36, 39, 46);
        v.widgets.hovered.bg_fill = Color32::from_rgb(50, 54, 63);
        v.widgets.hovered.weak_bg_fill = Color32::from_rgb(50, 54, 63);
        v.widgets.active.bg_fill = Color32::from_rgb(62, 66, 76);
        v.widgets.active.weak_bg_fill = Color32::from_rgb(62, 66, 76);
        v.widgets.inactive.fg_stroke = Stroke::new(1.0, TEXT);
        v.widgets.hovered.bg_stroke = Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 255, 255, 40));
    });
}

//! Vector icons drawn with the egui painter on a 24×24 design grid — crisp at any DPI,
//! no icon font or image assets.

use crate::theme;
use egui::{pos2, Color32, Painter, Pos2, Rect, Response, Sense, Shape, Stroke, Ui, Vec2};
use std::f32::consts::TAU;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)] // some icons are only used on one platform (Folder: native, Upload: web)
pub enum Icon {
    Play,
    Pause,
    Fullscreen,
    ExitFullscreen,
    Lens,
    Info,
    Edit,
    Close,
    Back,
    Plus,
    Trash,
    Check,
    Upload,
    Folder,
    Undo,
    Redo,
    Target,
    Area,
    Pin,
    Arrow,
}

pub fn paint(p: &Painter, rect: Rect, icon: Icon, color: Color32) {
    let s = rect.width().min(rect.height()) / 24.0;
    let c = rect.center();
    let pt = |x: f32, y: f32| pos2(c.x + (x - 12.0) * s, c.y + (y - 12.0) * s);
    let st = Stroke::new(1.9 * s, color);
    let line = |pts: &[(f32, f32)]| {
        p.add(Shape::line(pts.iter().map(|&(x, y)| pt(x, y)).collect(), st));
    };
    let arc = |cx: f32, cy: f32, r: f32, a0: f32, a1: f32| -> Vec<Pos2> {
        let n = 24;
        (0..=n)
            .map(|i| {
                let a = a0 + (a1 - a0) * i as f32 / n as f32;
                pt(cx + r * a.cos(), cy + r * a.sin())
            })
            .collect()
    };
    match icon {
        Icon::Play => {
            p.add(Shape::convex_polygon(vec![pt(8.5, 5.5), pt(18.5, 12.0), pt(8.5, 18.5)], color, Stroke::NONE));
        }
        Icon::Pause => {
            p.rect_filled(Rect::from_min_max(pt(7.0, 5.5), pt(10.5, 18.5)), 1.5 * s, color);
            p.rect_filled(Rect::from_min_max(pt(13.5, 5.5), pt(17.0, 18.5)), 1.5 * s, color);
        }
        Icon::Fullscreen => {
            line(&[(4.5, 9.5), (4.5, 4.5), (9.5, 4.5)]);
            line(&[(14.5, 4.5), (19.5, 4.5), (19.5, 9.5)]);
            line(&[(19.5, 14.5), (19.5, 19.5), (14.5, 19.5)]);
            line(&[(9.5, 19.5), (4.5, 19.5), (4.5, 14.5)]);
        }
        Icon::ExitFullscreen => {
            line(&[(9.5, 4.5), (9.5, 9.5), (4.5, 9.5)]);
            line(&[(14.5, 4.5), (14.5, 9.5), (19.5, 9.5)]);
            line(&[(19.5, 14.5), (14.5, 14.5), (14.5, 19.5)]);
            line(&[(4.5, 14.5), (9.5, 14.5), (9.5, 19.5)]);
        }
        Icon::Lens => {
            p.circle_stroke(pt(12.0, 12.0), 8.0 * s, st);
            let mut e = vec![];
            for i in 0..=32 {
                let a = i as f32 / 32.0 * TAU;
                e.push(pt(12.0 + 3.6 * a.cos(), 12.0 + 8.0 * a.sin()));
            }
            p.add(Shape::line(e, Stroke::new(1.4 * s, color)));
            line(&[(4.0, 12.0), (20.0, 12.0)]);
        }
        Icon::Info => {
            p.circle_stroke(pt(12.0, 12.0), 8.5 * s, st);
            p.circle_filled(pt(12.0, 8.0), 1.3 * s, color);
            line(&[(12.0, 11.0), (12.0, 16.5)]);
        }
        Icon::Edit => {
            line(&[(5.0, 19.0), (6.0, 15.0), (15.5, 5.5), (18.5, 8.5), (9.0, 18.0), (5.0, 19.0)]);
            line(&[(13.5, 7.5), (16.5, 10.5)]);
        }
        Icon::Close => {
            line(&[(6.5, 6.5), (17.5, 17.5)]);
            line(&[(17.5, 6.5), (6.5, 17.5)]);
        }
        Icon::Back => line(&[(14.5, 5.5), (8.0, 12.0), (14.5, 18.5)]),
        Icon::Plus => {
            line(&[(12.0, 5.0), (12.0, 19.0)]);
            line(&[(5.0, 12.0), (19.0, 12.0)]);
        }
        Icon::Trash => {
            line(&[(4.5, 7.0), (19.5, 7.0)]);
            line(&[(9.5, 7.0), (9.5, 4.5), (14.5, 4.5), (14.5, 7.0)]);
            line(&[(6.5, 7.0), (7.5, 19.5), (16.5, 19.5), (17.5, 7.0)]);
        }
        Icon::Check => line(&[(5.0, 12.5), (10.0, 17.5), (19.0, 7.0)]),
        Icon::Upload => {
            line(&[(12.0, 15.0), (12.0, 4.5)]);
            line(&[(8.0, 8.5), (12.0, 4.5), (16.0, 8.5)]);
            line(&[(4.5, 15.5), (4.5, 19.5), (19.5, 19.5), (19.5, 15.5)]);
        }
        Icon::Folder => line(&[(3.5, 7.0), (3.5, 18.5), (20.5, 18.5), (20.5, 8.5), (11.5, 8.5), (9.5, 5.5), (3.5, 5.5), (3.5, 7.0)]),
        Icon::Undo => {
            p.add(Shape::line(arc(13.0, 13.0, 6.5, -2.6, 1.2), st));
            line(&[(4.5, 7.5), (6.8, 10.6), (10.0, 8.6)]);
        }
        Icon::Redo => {
            p.add(Shape::line(arc(11.0, 13.0, 6.5, 1.95, 5.75), st));
            line(&[(19.5, 7.5), (17.2, 10.6), (14.0, 8.6)]);
        }
        Icon::Target => {
            p.circle_stroke(pt(12.0, 12.0), 7.0 * s, st);
            p.circle_filled(pt(12.0, 12.0), 2.0 * s, color);
            line(&[(12.0, 2.5), (12.0, 5.5)]);
            line(&[(12.0, 18.5), (12.0, 21.5)]);
            line(&[(2.5, 12.0), (5.5, 12.0)]);
            line(&[(18.5, 12.0), (21.5, 12.0)]);
        }
        Icon::Area => {
            line(&[(5.0, 7.0), (17.0, 4.5), (19.5, 16.0), (8.0, 19.5), (5.0, 7.0)]);
            for (x, y) in [(5.0, 7.0), (17.0, 4.5), (19.5, 16.0), (8.0, 19.5)] {
                p.circle_filled(pt(x, y), 1.8 * s, color);
            }
        }
        Icon::Pin => {
            let mut pts = arc(12.0, 9.5, 5.5, 2.6, TAU + 0.54);
            pts.push(pt(12.0, 20.0));
            pts.push(pts[0]);
            p.add(Shape::line(pts, st));
            p.circle_filled(pt(12.0, 9.5), 1.8 * s, color);
        }
        Icon::Arrow => {
            p.circle_stroke(pt(12.0, 12.0), 8.5 * s, st);
            line(&[(8.0, 14.0), (12.0, 9.5), (16.0, 14.0)]);
        }
    }
}

/// A round glass icon button. `active` highlights it with the accent colour.
pub fn button(ui: &mut Ui, icon: Icon, tooltip: &str, active: bool) -> Response {
    let size = Vec2::splat(40.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let t = ui.ctx().animate_bool(resp.id, resp.hovered());
    let fill = lerp_color(Color32::TRANSPARENT, theme::glass_hover(), t);
    ui.painter().circle_filled(rect.center(), 20.0, fill);
    let color = if active { theme::ACCENT } else { lerp_color(Color32::from_gray(225), Color32::WHITE, t) };
    paint(ui.painter(), rect.shrink(9.0), icon, color);
    resp.on_hover_text(tooltip)
}

/// Width a [`pill`] with this label will take.
pub fn pill_width(ui: &Ui, label: &str) -> f32 {
    ui.painter().layout_no_wrap(label.to_string(), theme::body_font(14.0), Color32::WHITE).size().x + 52.0
}

/// Starts a row of pills centred in the available width (`labels` = the pills that follow).
pub fn center_row(ui: &mut Ui, labels: &[&str]) {
    let gap = ui.spacing().item_spacing.x;
    let total: f32 = labels.iter().map(|l| pill_width(ui, l)).sum::<f32>() + gap * (labels.len().saturating_sub(1)) as f32;
    ui.add_space(((ui.available_width() - total) / 2.0).max(0.0));
}

/// A pill button with an icon and a label.
pub fn pill(ui: &mut Ui, icon: Icon, label: &str, primary: bool) -> Response {
    let font = theme::body_font(14.0);
    let galley = ui.painter().layout_no_wrap(label.to_string(), font, Color32::WHITE);
    let size = egui::vec2(galley.size().x + 52.0, 36.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let t = ui.ctx().animate_bool(resp.id, resp.hovered());
    let (bg, fg) = if primary {
        (lerp_color(theme::ACCENT, Color32::from_rgb(255, 205, 120), t), Color32::from_rgb(25, 20, 10))
    } else {
        (lerp_color(Color32::from_rgb(38, 41, 49), Color32::from_rgb(54, 58, 68), t), theme::TEXT)
    };
    ui.painter().rect_filled(rect, 18.0, bg);
    paint(ui.painter(), Rect::from_center_size(pos2(rect.left() + 22.0, rect.center().y), Vec2::splat(18.0)), icon, fg);
    ui.painter().galley_with_override_text_color(pos2(rect.left() + 38.0, rect.center().y - galley.size().y / 2.0), galley, fg);
    resp
}

pub fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgba_premultiplied(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()), l(a.a(), b.a()))
}

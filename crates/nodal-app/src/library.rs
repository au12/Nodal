//! Start screen: native (drop zone, recent tours) and web (server gallery, uploads).

use crate::app::{Cmd, NodalApp};
use crate::icons::{self, Icon};
use crate::loader::TourSummary;
use crate::theme;
use egui::{pos2, vec2, Align2, Color32, Rect, RichText, Sense, Shape, Stroke, Ui};

#[derive(Default)]
pub struct Library {
    pub tours: Option<Vec<TourSummary>>,
    /// Background job progress (tiling or uploading).
    pub job: Option<(f32, String)>,
    pub url_input: String,
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    confirm_delete: Option<String>,
}

pub fn show(app: &mut NodalApp, ui: &mut Ui) {
    let ctx = ui.ctx().clone();
    egui::CentralPanel::no_frame().show(ui, |ui| {
        let rect = ui.max_rect();
        backdrop(ui, rect);
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            let width = (rect.width() - 48.0).min(1120.0);
            ui.add_space(rect.height().min(900.0) * 0.07);
            ui.vertical_centered(|ui| {
                ui.set_max_width(width);
                header(ui);
                ui.add_space(26.0);
                if let Some((p, stage)) = app.library.job.clone() {
                    progress_card(ui, p, &stage, width);
                    ui.add_space(18.0);
                }
                #[cfg(not(target_arch = "wasm32"))]
                native_body(app, ui, width);
                #[cfg(target_arch = "wasm32")]
                web_body(app, ui, width);
            });
            ui.add_space(40.0);
        });
    });
    let _ = ctx;
}

fn backdrop(ui: &Ui, rect: Rect) {
    let p = ui.painter();
    p.rect_filled(rect, 0.0, theme::BG);
    // Soft warm glow behind the header.
    let c = pos2(rect.center().x, rect.top() + 120.0);
    for i in 0..18 {
        let r = 60.0 + i as f32 * 34.0;
        let a = (14.0 * (1.0 - i as f32 / 18.0)) as u8;
        p.circle_filled(c, r, Color32::from_rgba_unmultiplied(255, 170, 60, a / 3));
    }
}

fn logo(ui: &Ui, center: egui::Pos2, r: f32) {
    let p = ui.painter();
    p.circle_stroke(center, r, Stroke::new(r * 0.12, theme::ACCENT));
    // Meridian ellipse + equator — a sphere seen from its nodal point.
    let mut e = vec![];
    for i in 0..=48 {
        let a = i as f32 / 48.0 * std::f32::consts::TAU;
        e.push(center + vec2(r * 0.42 * a.cos(), r * a.sin()));
    }
    p.add(Shape::line(e, Stroke::new(r * 0.08, theme::ACCENT)));
    p.line_segment([center - vec2(r, 0.0), center + vec2(r, 0.0)], Stroke::new(r * 0.08, theme::ACCENT));
    p.circle_filled(center, r * 0.16, Color32::WHITE);
}

fn header(ui: &mut Ui) {
    let (rect, _) = ui.allocate_exact_size(vec2(64.0, 64.0), Sense::hover());
    logo(ui, rect.center(), 26.0);
    ui.add_space(6.0);
    ui.label(RichText::new("Nodal").font(theme::heading_font(34.0)));
    ui.label(RichText::new("Fast, GPU-accelerated 360° panoramas and virtual tours").size(15.0).color(theme::MUTED));
}

fn progress_card(ui: &mut Ui, p: f32, stage: &str, width: f32) {
    theme::glass_frame().inner_margin(16).show(ui, |ui| {
        ui.set_width(width.min(560.0));
        ui.label(RichText::new(stage).color(theme::TEXT));
        ui.add(egui::ProgressBar::new(p.clamp(0.0, 1.0)).show_percentage().fill(theme::ACCENT_DIM));
    });
    ui.ctx().request_repaint();
}

/// A tour card: thumbnail, title, meta. Returns (clicked, delete_clicked).
fn card(ui: &mut Ui, title: &str, meta: &str, thumb: Option<&str>, deletable: bool) -> (bool, bool) {
    let size = vec2(256.0, 206.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let t = ui.ctx().animate_bool(resp.id, resp.hovered());
    let r = rect.translate(vec2(0.0, -3.0 * t));
    let p = ui.painter();
    p.rect_filled(r.translate(vec2(0.0, 4.0)), 16.0, Color32::from_black_alpha((40.0 + 40.0 * t) as u8));
    p.rect_filled(r, 16.0, Color32::from_rgb(24, 26, 31));
    let img = Rect::from_min_size(r.min, vec2(r.width(), 144.0));
    match thumb {
        Some(uri) => {
            egui::Image::new(uri.to_string()).corner_radius(egui::CornerRadius { nw: 16, ne: 16, sw: 0, se: 0 }).paint_at(ui, img);
        }
        None => {
            ui.painter().rect_filled(img, egui::CornerRadius { nw: 16, ne: 16, sw: 0, se: 0 }, Color32::from_rgb(40, 44, 52));
        }
    }
    let p = ui.painter();
    p.text(pos2(r.left() + 14.0, img.bottom() + 12.0), Align2::LEFT_TOP, title, theme::heading_font(15.0), theme::TEXT);
    p.text(pos2(r.left() + 14.0, img.bottom() + 34.0), Align2::LEFT_TOP, meta, theme::body_font(12.0), theme::MUTED);
    p.rect_stroke(r, 16.0, Stroke::new(1.0, Color32::from_white_alpha((18.0 + 50.0 * t) as u8)), egui::StrokeKind::Inside);
    let mut delete = false;
    if deletable && t > 0.0 {
        let b = Rect::from_center_size(pos2(r.right() - 22.0, r.top() + 22.0), vec2(32.0, 32.0));
        let bresp = ui.interact(b, resp.id.with("del"), Sense::click());
        ui.painter().circle_filled(b.center(), 16.0, Color32::from_black_alpha(if bresp.hovered() { 200 } else { 140 }));
        icons::paint(ui.painter(), b.shrink(8.0), Icon::Trash, if bresp.hovered() { theme::DANGER } else { Color32::WHITE });
        delete = bresp.clicked();
    }
    (resp.clicked() && !delete, delete)
}

fn grid(ui: &mut Ui, width: f32, n: usize, mut item: impl FnMut(&mut Ui, usize)) {
    let per_row = ((width + 16.0) / (256.0 + 16.0)).floor().max(1.0) as usize;
    let row_w = per_row.min(n.max(1)) as f32 * 272.0 - 16.0;
    for row in 0..n.div_ceil(per_row) {
        ui.allocate_ui_with_layout(vec2(row_w, 210.0), egui::Layout::left_to_right(egui::Align::TOP), |ui| {
            ui.spacing_mut().item_spacing.x = 16.0;
            for i in row * per_row..((row + 1) * per_row).min(n) {
                item(ui, i);
            }
        });
        ui.add_space(14.0);
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn native_body(app: &mut NodalApp, ui: &mut Ui, width: f32) {
    // Drop zone.
    let zone_w = width.min(720.0);
    let hovering_files = ui.ctx().input(|i| !i.raw.hovered_files.is_empty());
    theme::glass_frame().inner_margin(26).show(ui, |ui| {
        ui.set_width(zone_w);
        let r = ui.max_rect().expand(14.0);
        dashed_rect(ui, r, if hovering_files { theme::ACCENT } else { Color32::from_white_alpha(40) });
        ui.vertical_centered(|ui| {
            ui.label(RichText::new("Drop photos, panoramas or a tour folder here").font(theme::heading_font(18.0)));
            ui.label(
                RichText::new("Overlapping photos are stitched into a panorama; finished panoramas become a tour.").color(theme::MUTED),
            );
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                icons::center_row(ui, &["Stitch photos…", "New tour from panoramas…", "Open tour…"]);
                if icons::pill(ui, Icon::Area, "Stitch photos…", true).clicked() {
                    crate::stitcher::open(app, Vec::new());
                }
                if icons::pill(ui, Icon::Plus, "New tour from panoramas…", false).clicked() {
                    if let Some(files) =
                        rfd::FileDialog::new().add_filter("Panoramas", &["jpg", "jpeg", "png", "tif", "tiff", "webp"]).pick_files()
                    {
                        new_tour_from(app, files);
                    }
                }
                if icons::pill(ui, Icon::Folder, "Open tour…", false).clicked() {
                    if let Some(dir) = rfd::FileDialog::new().set_title("Choose a tour folder").pick_folder() {
                        open_paths(app, vec![dir]);
                    }
                }
            });
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.add_space(((zone_w - 420.0) / 2.0).max(0.0));
                ui.add(egui::TextEdit::singleline(&mut app.library.url_input).hint_text("…or paste a tour URL").desired_width(330.0));
                if ui.button("Open").clicked() && !app.library.url_input.trim().is_empty() {
                    let mut url = app.library.url_input.trim().to_string();
                    if !url.ends_with(".json") {
                        url = format!("{}/tour.json", url.trim_end_matches('/'));
                    }
                    app.loader.open_url(url.clone(), crate::loader::Origin::Remote(url));
                }
            });
        });
    });
    if !app.persisted.recent.is_empty() {
        ui.add_space(30.0);
        ui.label(RichText::new("Recent tours").font(theme::heading_font(16.0)).color(theme::MUTED));
        ui.add_space(8.0);
        let recent = app.persisted.recent.clone();
        let mut remove = None;
        grid(ui, width, recent.len(), |ui, i| {
            let r = &recent[i];
            let meta = format!("{} scene{}", r.scenes, if r.scenes == 1 { "" } else { "s" });
            let (open, del) = card(ui, &r.title, &meta, r.thumb.as_deref(), true);
            if open {
                open_paths(app, vec![std::path::PathBuf::from(&r.path)]);
            }
            if del {
                remove = Some(i);
            }
        });
        if let Some(i) = remove {
            app.persisted.recent.remove(i);
        }
    }
}

#[cfg(target_arch = "wasm32")]
fn web_body(app: &mut NodalApp, ui: &mut Ui, width: f32) {
    let ctx = ui.ctx().clone();
    let Some(cfg) = app.server.clone() else {
        theme::glass_frame().inner_margin(22).show(ui, |ui| {
            ui.set_width(width.min(560.0));
            ui.label(RichText::new("No tour selected").font(theme::heading_font(17.0)));
            ui.label(
                RichText::new("Open this viewer with ?tour=path/to/tour.json, or run the Nodal server to manage tours here.")
                    .color(theme::MUTED),
            );
        });
        return;
    };
    if cfg.writable {
        ui.horizontal(|ui| {
            icons::center_row(ui, &["New tour from panoramas…"]);
            if icons::pill(ui, Icon::Upload, "New tour from panoramas…", true).clicked() {
                app.loader.pick_and_upload(&app.origin, None);
            }
        });
        if cfg.auth_required {
            ui.horizontal(|ui| {
                ui.add_space(((width - 330.0) / 2.0).max(0.0));
                ui.label(RichText::new("Access token").color(theme::MUTED));
                if ui.add(egui::TextEdit::singleline(&mut app.loader.token).password(true).desired_width(220.0)).changed() {
                    app.persisted.token = app.loader.token.clone();
                }
            });
        }
        ui.label(RichText::new("…or drop panoramas anywhere on this page").size(12.0).color(theme::MUTED));
    }
    ui.add_space(24.0);
    match app.library.tours.clone() {
        None => {
            ui.spinner();
        }
        Some(list) if list.is_empty() => {
            ui.label(RichText::new("No tours yet").font(theme::heading_font(16.0)).color(theme::MUTED));
        }
        Some(list) => {
            let mut delete = None;
            grid(ui, width, list.len(), |ui, i| {
                let t = &list[i];
                let thumb = t.thumb.as_ref().map(|th| format!("{}/tours/{}/{}", app.origin, t.id, th));
                let meta = format!("{} scene{}", t.scenes, if t.scenes == 1 { "" } else { "s" });
                let (open, del) = card(ui, &t.title, &meta, thumb.as_deref(), cfg.writable);
                if open {
                    app.loader.server_open(&app.origin, &t.id);
                }
                if del {
                    delete = Some(t.id.clone());
                }
            });
            if let Some(id) = delete {
                app.library.confirm_delete = Some(id);
            }
        }
    }
    if let Some(id) = app.library.confirm_delete.clone() {
        let mut close = false;
        egui::Window::new("Delete tour?")
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .frame(theme::glass_frame().inner_margin(18))
            .show(&ctx, |ui| {
                ui.label("This removes the tour and all of its tiles from the server.");
                ui.horizontal(|ui| {
                    if ui.add(egui::Button::new(RichText::new("Delete").color(theme::DANGER))).clicked() {
                        app.loader.server_delete(&app.origin, &id);
                        close = true;
                    }
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                });
            });
        if close {
            app.library.confirm_delete = None;
        }
    }
}

#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
fn dashed_rect(ui: &Ui, r: Rect, color: Color32) {
    let stroke = Stroke::new(1.5, color);
    let pts = [r.left_top(), r.right_top(), r.right_bottom(), r.left_bottom(), r.left_top()];
    for w in pts.windows(2) {
        ui.painter().extend(Shape::dashed_line(&[w[0], w[1]], stroke, 8.0, 6.0));
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn is_image(p: &std::path::Path) -> bool {
    matches!(
        p.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref(),
        Some("jpg" | "jpeg" | "png" | "tif" | "tiff" | "webp")
    )
}

/// Native: opens dropped/selected paths — tour folders, tour.json files or panoramas.
#[cfg(not(target_arch = "wasm32"))]
pub fn open_paths(app: &mut NodalApp, paths: Vec<std::path::PathBuf>) {
    use crate::loader::Origin;
    let images: Vec<_> = paths.iter().filter(|p| is_image(p)).cloned().collect();
    // Several photos that are not finished 2:1 panoramas are a set to stitch.
    if images.len() >= 2 && !crate::stitcher::all_panoramas(&images) {
        crate::stitcher::open(app, images);
        return;
    }
    if !images.is_empty() {
        // Add to the open editable tour, or start a new one.
        if let (crate::app::Screen::Viewer, Some(Origin::Local(dir))) = (app.screen, app.tour.as_ref().map(|t| t.origin.clone())) {
            if app.editor.open {
                if app.editor.dirty {
                    app.save();
                }
                app.library.job = Some((0.0, "Tiling…".into()));
                app.loader.tile_local(images, dir, None, false);
                return;
            }
        }
        new_tour_from(app, images);
        return;
    }
    for p in paths {
        let (dir, json) = if p.is_dir() {
            (p.clone(), p.join("tour.json"))
        } else if p.extension().map(|e| e == "json").unwrap_or(false) {
            (p.parent().map(|d| d.to_path_buf()).unwrap_or_default(), p.clone())
        } else {
            continue;
        };
        if json.exists() {
            app.loader.open_url(json.to_string_lossy().to_string(), Origin::Local(dir));
            return;
        }
    }
    app.loader.report(crate::loader::Event::Error("Nothing to open: choose panoramas or a folder containing tour.json".into()));
}

#[cfg(not(target_arch = "wasm32"))]
fn new_tour_from(app: &mut NodalApp, images: Vec<std::path::PathBuf>) {
    let title = nodal_tiler::title_from_file(&images[0]);
    let base = crate::platform::tours_dir();
    let slug = nodal_core::tour::slugify(&title);
    let mut dir = base.join(&slug);
    let mut n = 2;
    while dir.exists() {
        dir = base.join(format!("{slug}-{n}"));
        n += 1;
    }
    app.library.job = Some((0.0, format!("Tiling {}…", title)));
    app.loader.tile_local(images, dir, Some(title), true);
}

#[cfg(target_arch = "wasm32")]
pub fn upload(app: &mut NodalApp, ctx: &egui::Context, files: Vec<(String, Vec<u8>)>) {
    if files.is_empty() {
        return;
    }
    if !app.server.as_ref().map(|s| s.writable).unwrap_or(false) {
        app.toast(ctx, "This server is read-only", true);
        return;
    }
    let tour = match (&app.screen, app.tour.as_ref().map(|t| &t.origin)) {
        (crate::app::Screen::Viewer, Some(crate::loader::Origin::Server(id))) if app.editor.open => Some(id.clone()),
        _ => None,
    };
    let title = crate::loader::title_from_name(&files[0].0);
    app.library.job = Some((0.0, "Uploading…".into()));
    app.loader.server_upload(&app.origin, files, tour, title);
}

/// "Add panoramas…" in the editor.
pub fn add_scenes_dialog(app: &mut NodalApp, ctx: &egui::Context) {
    let _ = ctx;
    #[cfg(not(target_arch = "wasm32"))]
    {
        if let Some(files) = rfd::FileDialog::new().add_filter("Panoramas", &["jpg", "jpeg", "png", "tif", "tiff", "webp"]).pick_files() {
            open_paths(app, files);
        }
    }
    #[cfg(target_arch = "wasm32")]
    {
        if let Some(crate::loader::Origin::Server(id)) = app.tour.as_ref().map(|t| t.origin.clone()) {
            if app.editor.dirty {
                app.save();
            }
            app.library.job = Some((0.0, "Uploading…".into()));
            app.loader.pick_and_upload(&app.origin, Some(id));
        }
    }
    let _ = Cmd::Reload;
}

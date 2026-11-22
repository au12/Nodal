//! Viewer overlays: title, scene strip, controls, compass, projection menu, info cards, help.

use crate::app::{Cmd, NodalApp};
use crate::icons::{self, Icon};
use crate::theme;
use egui::{pos2, vec2, Align, Align2, Color32, Id, Layout, Order, Rect, RichText, Sense, Shape, Stroke, Ui, Vec2};
use nodal_core::{ProjMode, Projection};

pub fn overlays(app: &mut NodalApp, ctx: &egui::Context) {
    let alpha = app.ui.ui_alpha;
    loading_bar(app, ctx);
    if alpha > 0.03 {
        title_block(app, ctx, alpha);
        bottom_bar(app, ctx, alpha);
        compass(app, ctx, alpha);
        if app.ui.show_projection {
            projection_menu(app, ctx);
        }
    }
    if app.ui.show_stats {
        stats(app, ctx);
    }
    if app.ui.show_help {
        help(app, ctx);
    }
    info_card(app, ctx);
}

fn screen(ctx: &egui::Context) -> Rect {
    ctx.content_rect()
}

/// The area overlays should stay inside: the panorama view (excludes the editor panel).
fn pano(app: &NodalApp, ctx: &egui::Context) -> Rect {
    app.ui.pano_rect.unwrap_or_else(|| ctx.content_rect())
}

fn loading_bar(app: &NodalApp, ctx: &egui::Context) {
    let st = app.status;
    let busy = !st.is_complete() && st.wanted_on_screen > 0;
    let t = ctx.animate_bool_with_time(Id::unique("loadbar"), busy, 0.4);
    if t <= 0.0 {
        return;
    }
    let r = pano(app, ctx);
    let p = ctx.layer_painter(egui::LayerId::new(Order::Foreground, Id::unique("loadbar_layer")));
    let w = r.width() * (0.08 + 0.92 * st.progress());
    let a = (t * 255.0) as u8;
    p.rect_filled(Rect::from_min_size(r.min, vec2(w, 2.5)), 0.0, Color32::from_rgba_unmultiplied(255, 181, 71, a));
}

fn title_block(app: &mut NodalApp, ctx: &egui::Context, alpha: f32) {
    let Some(t) = &app.tour else { return };
    let Some(scene) = t.tour.scenes.get(app.scene) else { return };
    let tour_title = t.tour.title.clone();
    let scene_title = if scene.title.is_empty() { scene.id.clone() } else { scene.title.clone() };
    let show_back = !app.static_mode;
    let r = pano(app, ctx);
    let narrow = r.width() < 520.0;
    egui::Area::new(Id::unique("title_block"))
        .pivot(Align2::LEFT_TOP)
        .fixed_pos(r.left_top() + vec2(16.0, 16.0))
        .order(Order::Middle)
        .show(ctx, |ui| {
            ui.set_opacity(alpha);
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
            theme::glass_frame().inner_margin(egui::Margin { left: 8, right: 16, top: 6, bottom: 6 }).show(ui, |ui| {
                ui.horizontal(|ui| {
                    if show_back && icons::button(ui, Icon::Back, "Back to library", false).clicked() {
                        app.cmds.push(Cmd::BackToLibrary);
                    }
                    let heading = RichText::new(scene_title).font(theme::heading_font(if narrow { 15.0 } else { 17.0 }));
                    if narrow || tour_title.is_empty() {
                        ui.label(heading);
                    } else {
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 1.0;
                            ui.label(RichText::new(tour_title).size(11.5).color(theme::MUTED));
                            ui.label(heading);
                        });
                    }
                });
            });
        });
}

fn bottom_bar(app: &mut NodalApp, ctx: &egui::Context, alpha: f32) {
    let Some(t) = &app.tour else { return };
    let scenes: Vec<(String, Option<String>)> = t
        .tour
        .scenes
        .iter()
        .map(|s| {
            (
                if s.title.is_empty() { s.id.clone() } else { s.title.clone() },
                s.thumb.as_ref().map(|th| crate::loader::asset_uri(&t.base_url, th)),
            )
        })
        .collect();
    let has_description =
        !t.tour.description.is_empty() || t.tour.scenes.get(app.scene).map(|s| !s.description.is_empty()).unwrap_or(false);
    let r = pano(app, ctx);
    let screen_w = r.width();
    let compact = screen_w < 700.0;
    let max_w = (screen_w - 32.0).min(1100.0);
    egui::Area::new(Id::unique("bottom_bar"))
        .pivot(Align2::CENTER_BOTTOM)
        .fixed_pos(r.center_bottom() + vec2(0.0, -16.0))
        .order(Order::Middle)
        .show(ctx, |ui| {
            ui.set_opacity(alpha);
            ui.set_max_width(max_w);
            theme::glass_frame().show(ui, |ui| {
                ui.set_max_width(max_w - 20.0);
                let strip = |ui: &mut Ui, app: &mut NodalApp| {
                    if scenes.len() > 1 {
                        let thumb = if compact { vec2(84.0, 48.0) } else { vec2(112.0, 63.0) };
                        egui::ScrollArea::horizontal()
                            .id_salt("scene_strip")
                            .max_width(if compact { max_w - 20.0 } else { max_w - 260.0 })
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    for (i, (title, uri)) in scenes.iter().enumerate() {
                                        if scene_thumb(ui, title, uri.as_deref(), thumb, i == app.scene).clicked() && i != app.scene {
                                            app.cmds.push(Cmd::Goto { scene: i, view: None, focus: None });
                                        }
                                    }
                                });
                            });
                    }
                };
                let buttons = |ui: &mut Ui, app: &mut NodalApp| {
                    let rotating = app.camera.autorotate_on;
                    if icons::button(
                        ui,
                        if rotating { Icon::Pause } else { Icon::Play },
                        if rotating { "Pause auto-rotation (Space)" } else { "Auto-rotate (Space)" },
                        false,
                    )
                    .clicked()
                    {
                        app.camera.set_autorotate(!rotating);
                    }
                    if icons::button(ui, Icon::Lens, "Projection (P)", app.ui.show_projection).clicked() {
                        app.ui.show_projection = !app.ui.show_projection;
                    }
                    if has_description && icons::button(ui, Icon::Info, "About this place", false).clicked() {
                        if let Some(t) = &app.tour {
                            let s = &t.tour.scenes[app.scene];
                            let (title, text) = if !s.description.is_empty() {
                                (s.title.clone(), s.description.clone())
                            } else {
                                (t.tour.title.clone(), t.tour.description.clone())
                            };
                            app.cmds.push(Cmd::Info { title, text });
                        }
                    }
                    if app.can_edit() && icons::button(ui, Icon::Edit, "Edit tour (E)", app.editor.open).clicked() {
                        app.editor.open = !app.editor.open;
                    }
                    let full = crate::platform::is_fullscreen(ui.ctx());
                    if icons::button(ui, if full { Icon::ExitFullscreen } else { Icon::Fullscreen }, "Fullscreen (F)", false).clicked() {
                        app.cmds.push(Cmd::Fullscreen);
                    }
                };
                if compact {
                    let n = 3.0 + if has_description { 1.0 } else { 0.0 } + if app.can_edit() { 1.0 } else { 0.0 };
                    ui.vertical_centered(|ui| {
                        strip(ui, app);
                        ui.horizontal(|ui| {
                            let w = n * 40.0 + (n - 1.0) * ui.spacing().item_spacing.x;
                            ui.add_space(((ui.available_width() - w) / 2.0).max(0.0));
                            buttons(ui, app)
                        });
                    });
                } else {
                    ui.horizontal(|ui| {
                        strip(ui, app);
                        if scenes.len() > 1 {
                            ui.add_space(4.0);
                            let (r, _) = ui.allocate_exact_size(vec2(1.0, 36.0), Sense::hover());
                            ui.painter().rect_filled(r, 0.0, Color32::from_white_alpha(24));
                            ui.add_space(4.0);
                        }
                        buttons(ui, app);
                    });
                }
            });
        });
}

fn scene_thumb(ui: &mut Ui, title: &str, uri: Option<&str>, size: Vec2, current: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let hover = ui.ctx().animate_bool(resp.id, resp.hovered() || current);
    let r = rect.shrink(2.0 * (1.0 - hover));
    match uri {
        Some(uri) => {
            egui::Image::new(uri.to_string()).corner_radius(9).paint_at(ui, r);
        }
        None => {
            ui.painter().rect_filled(r, 9.0, Color32::from_rgb(40, 44, 52));
        }
    }
    // Title on a soft gradient.
    let band = Rect::from_min_max(pos2(r.left(), r.bottom() - 22.0), r.max);
    let mut mesh = egui::Mesh::default();
    let top = Color32::TRANSPARENT;
    let bot = Color32::from_black_alpha(170);
    mesh.colored_vertex(band.left_top(), top);
    mesh.colored_vertex(band.right_top(), top);
    mesh.colored_vertex(band.right_bottom(), bot);
    mesh.colored_vertex(band.left_bottom(), bot);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(0, 2, 3);
    ui.painter().add(Shape::mesh(mesh));
    let mut job = egui::text::LayoutJob::simple_singleline(title.to_string(), theme::body_font(11.0), Color32::WHITE);
    job.wrap = egui::text::TextWrapping::truncate_at_width(r.width() - 12.0);
    let galley = ui.painter().layout_job(job);
    ui.painter().with_clip_rect(r).galley(pos2(r.left() + 6.0, r.bottom() - galley.size().y - 4.0), galley, Color32::WHITE);
    let stroke =
        if current { Stroke::new(2.0, theme::ACCENT) } else { Stroke::new(1.0, Color32::from_white_alpha((20.0 + 60.0 * hover) as u8)) };
    ui.painter().rect_stroke(r, 9.0, stroke, egui::StrokeKind::Outside);
    resp.on_hover_text(title)
}

fn compass(app: &mut NodalApp, ctx: &egui::Context, alpha: f32) {
    let Some(t) = &app.tour else { return };
    if !t.tour.settings.compass {
        return;
    }
    let north = t.tour.scenes.get(app.scene).map(|s| s.north).unwrap_or(0.0);
    let yaw = app.camera.view.yaw;
    let fov = app.camera.view.fov.min(180.0);
    let r = pano(app, ctx);
    egui::Area::new(Id::unique("compass")).pivot(Align2::RIGHT_TOP).fixed_pos(r.right_top() + vec2(-16.0, 16.0)).order(Order::Middle).show(
        ctx,
        |ui| {
            ui.set_opacity(alpha);
            let (rect, resp) = ui.allocate_exact_size(Vec2::splat(64.0), Sense::click());
            let c = rect.center();
            let p = ui.painter();
            p.circle_filled(c, 30.0, theme::glass());
            p.circle_stroke(c, 30.0, theme::hairline());
            // View cone (always pointing up).
            let half = (fov * 0.5).to_radians();
            let mut pts = vec![c];
            for i in 0..=16 {
                let a = -std::f32::consts::FRAC_PI_2 - half + 2.0 * half * i as f32 / 16.0;
                pts.push(c + vec2(a.cos(), a.sin()) * 24.0);
            }
            p.add(Shape::convex_polygon(pts, Color32::from_rgba_unmultiplied(255, 181, 71, 60), Stroke::NONE));
            // North marker, rotated by the view yaw.
            let a = (north - yaw).to_radians() - std::f32::consts::FRAC_PI_2;
            let dir = vec2(a.cos(), a.sin());
            let tip = c + dir * 22.0;
            p.circle_filled(tip, 8.0, theme::DANGER);
            p.text(tip, Align2::CENTER_CENTER, "N", theme::heading_font(10.0), Color32::WHITE);
            p.circle_filled(c, 2.5, Color32::WHITE);
            if resp.clicked() {
                let mut v = app.camera.view;
                v.yaw = north;
                app.cmds.push(Cmd::LookAt(v));
            }
            resp.on_hover_text("Face north");
        },
    );
}

/// Menu entry: label, projection, optional (pitch, fov) to animate to.
type Preset = (&'static str, Projection, Option<(f32, f32)>);

fn projection_menu(app: &mut NodalApp, ctx: &egui::Context) {
    let r = pano(app, ctx);
    egui::Area::new(Id::unique("projection_menu"))
        .pivot(Align2::RIGHT_BOTTOM)
        .fixed_pos(r.right_bottom() + vec2(-16.0, -108.0))
        .order(Order::Foreground)
        .show(ctx, |ui| {
            theme::glass_frame().inner_margin(14).show(ui, |ui| {
                ui.set_width(250.0f32.min(r.width() - 48.0));
                ui.label(RichText::new("Projection").font(theme::heading_font(15.0)));
                ui.add_space(4.0);
                let cur = app.camera.view.projection;
                let presets: [Preset; 5] = [
                    ("Rectilinear", Projection::RECTILINEAR, Some((0.0, 90.0))),
                    ("Fisheye", Projection::FISHEYE, Some((0.0, 150.0))),
                    ("Little planet", Projection::STEREOGRAPHIC, Some((-90.0, 290.0))),
                    ("Pannini", Projection::PANNINI, Some((0.0, 130.0))),
                    ("Flat overview", Projection::EQUIRECT, Some((0.0, 360.0))),
                ];
                for (name, p, hint) in presets {
                    let selected = cur.mode == p.mode && (cur.distortion - p.distortion).abs() < 0.2;
                    if ui.add_sized([ui.available_width(), 30.0], egui::Button::selectable(selected, name)).clicked() {
                        app.cmds.push(Cmd::Projection(p, hint));
                    }
                }
                ui.add_space(6.0);
                ui.separator();
                let mut v = app.camera.view;
                let mut changed = false;
                if v.projection.mode != ProjMode::Equirect {
                    ui.label(RichText::new("Distortion").color(theme::MUTED).size(12.0));
                    changed |= ui.add(egui::Slider::new(&mut v.projection.distortion, 0.0..=1.0).show_value(false)).changed();
                }
                if v.projection.mode == ProjMode::Perspective {
                    ui.label(RichText::new("Architectural (straight verticals)").color(theme::MUTED).size(12.0));
                    changed |= ui.add(egui::Slider::new(&mut v.projection.architectural, 0.0..=1.0).show_value(false)).changed();
                }
                if changed {
                    app.camera.jump_to(v);
                }
                ui.separator();
                ui.checkbox(&mut app.persisted.show_labels, "Always show hotspot labels");
                ui.checkbox(&mut app.persisted.saver, "Data saver (lower resolution)");
            });
        });
}

fn stats(app: &NodalApp, ctx: &egui::Context) {
    let st = app.status;
    let v = app.camera.view;
    let dt = ctx.input(|i| i.stable_dt).max(1e-4);
    let r = pano(app, ctx);
    egui::Area::new(Id::unique("stats"))
        .pivot(Align2::RIGHT_TOP)
        .fixed_pos(r.right_top() + vec2(-16.0, 92.0))
        .order(Order::Foreground)
        .show(ctx, |ui| {
            theme::glass_frame().show(ui, |ui| {
                ui.style_mut().override_font_id = Some(egui::FontId::monospace(11.5));
                ui.label(&app.gpu_info);
                ui.label(format!("{:5.1} fps while drawing · {} frames in the last second", 1.0 / dt, app.frames_last_second));
                ui.label(format!("yaw {:7.2}  pitch {:6.2}  fov {:6.2}", v.yaw, v.pitch, v.fov));
                ui.label(format!("{} d={:.2} arch={:.2}", v.projection.label(), v.projection.distortion, v.projection.architectural));
                ui.label(format!("visible tiles {}  missing {}  queued {}", st.wanted_on_screen, st.missing_on_screen, st.pending));
                ui.label(format!("lod debug (L): {}", ["off", "face uv", "levels", "base only"][app.debug_mode as usize % 4]));
            });
        });
}

fn help(app: &mut NodalApp, ctx: &egui::Context) {
    let mut open = true;
    egui::Window::new("Keyboard & mouse")
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
        .frame(theme::glass_frame().inner_margin(18))
        .open(&mut open)
        .show(ctx, |ui| {
            egui::Grid::new("help_grid").num_columns(2).spacing([24.0, 6.0]).show(ui, |ui| {
                for (k, d) in [
                    ("Drag / arrows / WASD", "Look around"),
                    ("Wheel / pinch / + −", "Zoom (towards the cursor)"),
                    ("Double-click", "Zoom in"),
                    ("1 … 9", "Jump to scene"),
                    ("Space", "Toggle auto-rotation"),
                    ("P", "Cycle projections"),
                    ("F", "Fullscreen"),
                    ("E", "Editor (when editable)"),
                    ("I / L", "Stats / level-of-detail overlay"),
                    ("Esc", "Close panels"),
                ] {
                    ui.label(RichText::new(k).font(theme::heading_font(13.0)));
                    ui.label(RichText::new(d).color(theme::MUTED));
                    ui.end_row();
                }
            });
        });
    if !open {
        app.ui.show_help = false;
    }
}

fn info_card(app: &mut NodalApp, ctx: &egui::Context) {
    let Some((title, text)) = app.ui.info.clone() else { return };
    let mut close = false;
    let r = pano(app, ctx);
    egui::Area::new(Id::unique("info_card")).pivot(Align2::CENTER_CENTER).fixed_pos(r.center()).order(Order::Foreground).show(ctx, |ui| {
        theme::glass_frame().inner_margin(20).show(ui, |ui| {
            ui.set_width(440.0f32.min(r.width() - 64.0));
            ui.horizontal(|ui| {
                ui.label(RichText::new(if title.is_empty() { "Info" } else { &title }).font(theme::heading_font(19.0)));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if icons::button(ui, Icon::Close, "Close (Esc)", false).clicked() {
                        close = true;
                    }
                });
            });
            ui.add_space(4.0);
            egui::ScrollArea::vertical().max_height(r.height() * 0.5).show(ui, |ui| {
                ui.label(RichText::new(text).size(14.5).color(Color32::from_gray(225)));
            });
        });
    });
    if close {
        app.ui.info = None;
    }
}

pub fn toasts(app: &mut NodalApp, ctx: &egui::Context) {
    let now = ctx.input(|i| i.time);
    app.toasts.retain(|t| t.until > now);
    if app.toasts.is_empty() {
        return;
    }
    ctx.request_repaint_after(std::time::Duration::from_millis(250));
    let r = if app.screen == crate::app::Screen::Viewer { pano(app, ctx) } else { screen(ctx) };
    egui::Area::new(Id::unique("toasts")).pivot(Align2::CENTER_TOP).fixed_pos(r.center_top() + vec2(0.0, 18.0)).order(Order::Tooltip).show(
        ctx,
        |ui| {
            for t in &app.toasts {
                let fade = ((t.until - now) as f32 / 0.3).clamp(0.0, 1.0);
                ui.set_opacity(fade);
                theme::glass_frame().inner_margin(egui::Margin::symmetric(16, 10)).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        let (r, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
                        ui.painter().circle_filled(r.center(), 4.0, if t.error { theme::DANGER } else { theme::OK });
                        ui.label(&t.text);
                    });
                });
            }
        },
    );
}

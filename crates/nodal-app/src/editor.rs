//! Tour editor: scene settings, hotspot placement/editing, tour settings, undo/redo.

use crate::app::{new_point, Cmd, NodalApp};
use crate::hotspots::{self, ScreenMap};
use crate::icons::{self, Icon};
use crate::theme;
use egui::{Color32, CursorIcon, Key, RichText, Ui};
use nodal_core::math::{yaw_pitch_from_dir, DEG};
use nodal_core::tour::{Action, DragMode, Hotspot, HotspotKind, IconStyle, TransitionKind};
use nodal_core::{Projector, Tour};

#[derive(Clone, Debug, PartialEq)]
pub enum Placing {
    Point,
    Polygon(Vec<[f32; 2]>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Tab {
    #[default]
    Hotspots,
    Scene,
    Tour,
}

#[derive(Clone, Copy, Debug)]
enum DragTarget {
    Point(usize),
    Vertex(usize, usize),
}

#[derive(Default)]
pub struct Editor {
    pub open: bool,
    pub selected: Option<usize>,
    pub placing: Option<Placing>,
    pub dirty: bool,
    pub saving: bool,
    pub tab: Tab,
    undo: Vec<Tour>,
    redo: Vec<Tour>,
    committed: Option<Tour>,
    drag: Option<DragTarget>,
    confirm_delete_scene: Option<usize>,
}

impl Editor {
    pub fn reset(&mut self, tour: &Tour, keep_history: bool) {
        if !keep_history {
            self.undo.clear();
            self.redo.clear();
            self.selected = None;
            self.placing = None;
        }
        self.dirty = false;
        self.committed = Some(tour.clone());
    }

    /// Records a history step when the tour changed and no edit is in progress.
    fn commit(&mut self, tour: &Tour, ctx: &egui::Context) {
        let busy = ctx.egui_is_using_pointer() || ctx.egui_wants_keyboard_input();
        if busy {
            return;
        }
        if let Some(prev) = &self.committed {
            if prev != tour {
                self.undo.push(prev.clone());
                if self.undo.len() > 100 {
                    self.undo.remove(0);
                }
                self.redo.clear();
                self.committed = Some(tour.clone());
                self.dirty = true;
            }
        }
    }

    fn undo(&mut self, tour: &mut Tour) {
        if let Some(prev) = self.undo.pop() {
            self.redo.push(tour.clone());
            *tour = prev.clone();
            self.committed = Some(prev);
            self.dirty = true;
            self.selected = None;
        }
    }

    fn redo(&mut self, tour: &mut Tour) {
        if let Some(next) = self.redo.pop() {
            self.undo.push(tour.clone());
            *tour = next.clone();
            self.committed = Some(next);
            self.dirty = true;
            self.selected = None;
        }
    }
}

fn section(ui: &mut Ui, title: &str) {
    ui.add_space(10.0);
    ui.label(RichText::new(title).font(theme::heading_font(13.0)).color(theme::MUTED));
    ui.add_space(2.0);
}

/// A full-width, left-aligned selectable list row with a muted secondary text.
fn list_row(ui: &mut Ui, selected: bool, primary: &str, secondary: &str) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 30.0), egui::Sense::click());
    let t = ui.ctx().animate_bool(resp.id, resp.hovered());
    let fill = if selected {
        Color32::from_rgba_unmultiplied(255, 181, 71, 46)
    } else {
        crate::icons::lerp_color(Color32::TRANSPARENT, Color32::from_rgb(38, 41, 49), t)
    };
    ui.painter().rect_filled(rect, 8.0, fill);
    if selected {
        ui.painter().rect_filled(
            egui::Rect::from_min_size(rect.min + egui::vec2(0.0, 6.0), egui::vec2(3.0, rect.height() - 12.0)),
            2.0,
            theme::ACCENT,
        );
    }
    let painter = ui.painter().with_clip_rect(rect.shrink(4.0));
    let g = painter.layout_no_wrap(primary.to_string(), theme::body_font(13.5), theme::TEXT);
    let x = rect.left() + 12.0;
    let y = rect.center().y;
    let w = g.size().x;
    painter.galley(egui::pos2(x, y - g.size().y / 2.0), g, theme::TEXT);
    if !secondary.is_empty() {
        let g2 = painter.layout_no_wrap(secondary.to_string(), theme::body_font(12.0), theme::MUTED);
        painter.galley(egui::pos2(x + w + 10.0, y - g2.size().y / 2.0), g2, theme::MUTED);
    }
    resp
}

fn view_label(v: &nodal_core::View) -> String {
    format!("yaw {:.0}°, pitch {:.0}°, fov {:.0}°", v.yaw, v.pitch, v.fov)
}

pub fn side_panel(app: &mut NodalApp, ui: &mut Ui) {
    let ctx = ui.ctx().clone();
    let mut save = false;
    let mut add_scenes = false;
    egui::Panel::right("editor_panel")
        .exact_size(340.0)
        .resizable(false)
        .frame(egui::Frame::new().fill(theme::PANEL).inner_margin(14).stroke(theme::hairline()))
        .show(ui, |ui| {
            let NodalApp { tour, editor, camera, scene: scene_idx, cmds, .. } = app;
            let Some(lt) = tour.as_mut() else { return };
            let t = &mut lt.tour;
            ui.horizontal(|ui| {
                ui.label(RichText::new("Editor").font(theme::heading_font(18.0)));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if icons::button(ui, Icon::Close, "Close editor (Esc)", false).clicked() {
                        editor.open = false;
                    }
                    if icons::button(ui, Icon::Redo, "Redo (Ctrl+Shift+Z)", false).clicked() {
                        editor.redo(t);
                    }
                    if icons::button(ui, Icon::Undo, "Undo (Ctrl+Z)", false).clicked() {
                        editor.undo(t);
                    }
                });
            });
            ui.horizontal(|ui| {
                let label = if editor.saving {
                    "Saving…"
                } else if editor.dirty {
                    "Save changes"
                } else {
                    "Saved"
                };
                let resp = ui.add_enabled_ui(editor.dirty && !editor.saving, |ui| icons::pill(ui, Icon::Check, label, editor.dirty)).inner;
                if resp.clicked() {
                    save = true;
                }
                if editor.dirty {
                    ui.label(RichText::new("unsaved").color(theme::ACCENT).size(12.0));
                }
            });
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                for (tab, name) in [(Tab::Hotspots, "Hotspots"), (Tab::Scene, "Scene"), (Tab::Tour, "Tour")] {
                    if ui.add(egui::Button::selectable(editor.tab == tab, name)).clicked() {
                        editor.tab = tab;
                    }
                }
            });
            ui.separator();
            egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                ui.spacing_mut().slider_width = 180.0;
                let scene_ids: Vec<(String, String)> =
                    t.scenes.iter().map(|s| (s.id.clone(), if s.title.is_empty() { s.id.clone() } else { s.title.clone() })).collect();
                match editor.tab {
                    Tab::Hotspots => {
                        let Some(scene) = t.scenes.get_mut(*scene_idx) else { return };
                        ui.horizontal(|ui| {
                            if icons::pill(ui, Icon::Target, "Point", editor.placing == Some(Placing::Point)).clicked() {
                                editor.placing = Some(Placing::Point);
                            }
                            if icons::pill(ui, Icon::Area, "Area", matches!(editor.placing, Some(Placing::Polygon(_)))).clicked() {
                                editor.placing = Some(Placing::Polygon(vec![]));
                            }
                        });
                        let hint = match &editor.placing {
                            Some(Placing::Point) => "Click in the panorama to place the hotspot.",
                            Some(Placing::Polygon(p)) if p.len() < 3 => "Click to add corners (at least 3).",
                            Some(Placing::Polygon(_)) => "Double-click or press Enter to finish, Esc to cancel.",
                            None => "Drag hotspots in the view to move them. Click one to edit it.",
                        };
                        ui.label(RichText::new(hint).size(12.0).color(theme::MUTED));
                        section(ui, &format!("Hotspots in this scene ({})", scene.hotspots.len()));
                        let titles = |id: &str| {
                            scene_ids.iter().find(|(i, _)| i == id).map(|(_, t)| t.clone()).unwrap_or_else(|| format!("missing '{id}'"))
                        };
                        for (i, h) in scene.hotspots.iter().enumerate() {
                            let sel = editor.selected == Some(i);
                            let name = if h.label.is_empty() { h.id.clone() } else { h.label.clone() };
                            if list_row(ui, sel, &name, &hotspots::describe(&h.action, titles)).clicked() {
                                editor.selected = if sel { None } else { Some(i) };
                                if !sel {
                                    let (yaw, pitch) = h.kind.anchor();
                                    cmds.push(Cmd::LookAt(nodal_core::View { yaw, pitch, ..camera.view }));
                                }
                            }
                        }
                        if let Some(i) = editor.selected {
                            if i < scene.hotspots.len() {
                                section(ui, "Selected hotspot");
                                if hotspot_props(ui, &mut scene.hotspots[i], &scene_ids, camera.view) {
                                    scene.hotspots.remove(i);
                                    editor.selected = None;
                                }
                            }
                        }
                    }
                    Tab::Scene => {
                        let n = t.scenes.len();
                        if let Some(scene) = t.scenes.get_mut(*scene_idx) {
                            section(ui, "Scene");
                            ui.label("Title");
                            ui.add(egui::TextEdit::singleline(&mut scene.title).desired_width(f32::INFINITY));
                            ui.label("Description");
                            ui.add(egui::TextEdit::multiline(&mut scene.description).desired_rows(3).desired_width(f32::INFINITY));
                            section(ui, "Start view");
                            ui.label(RichText::new(view_label(&scene.view)).size(12.0).color(theme::MUTED));
                            if ui.button("Use current view").clicked() {
                                scene.view = camera.view;
                            }
                            section(ui, "Compass");
                            ui.horizontal(|ui| {
                                ui.add(egui::DragValue::new(&mut scene.north).range(-180.0..=180.0).suffix("° north"));
                                if ui.button("North is where I'm looking").clicked() {
                                    scene.north = camera.view.yaw;
                                }
                            });
                            section(ui, "Limits");
                            egui::Grid::new("limits").num_columns(2).show(ui, |ui| {
                                ui.label("Zoom (fov)");
                                ui.horizontal(|ui| {
                                    ui.add(egui::DragValue::new(&mut scene.limits.min_fov).range(1.0..=90.0).suffix("°"));
                                    ui.label("–");
                                    ui.add(egui::DragValue::new(&mut scene.limits.max_fov).range(30.0..=170.0).suffix("°"));
                                });
                                ui.end_row();
                                ui.label("Pitch");
                                ui.horizontal(|ui| {
                                    ui.add(egui::DragValue::new(&mut scene.limits.min_pitch).range(-90.0..=0.0).suffix("°"));
                                    ui.label("–");
                                    ui.add(egui::DragValue::new(&mut scene.limits.max_pitch).range(0.0..=90.0).suffix("°"));
                                });
                                ui.end_row();
                                ui.label("Max pixel zoom");
                                ui.add(egui::DragValue::new(&mut scene.limits.max_pixel_zoom).range(0.5..=8.0).speed(0.05).suffix("×"));
                                ui.end_row();
                            });
                            camera.limits = scene.limits;
                        }
                        section(ui, &format!("Scenes ({n})"));
                        let mut action: Option<(usize, i32)> = None;
                        for i in 0..n {
                            let s = &t.scenes[i];
                            let is_start = t.start_scene.as_deref() == Some(s.id.as_str()) || (t.start_scene.is_none() && i == 0);
                            ui.horizontal(|ui| {
                                let title = if s.title.is_empty() { s.id.clone() } else { s.title.clone() };
                                let row = ui
                                    .allocate_ui(egui::vec2(170.0, 30.0), |ui| {
                                        list_row(ui, i == *scene_idx, &title, if is_start { "start" } else { "" })
                                    })
                                    .inner;
                                if row.clicked() && i != *scene_idx {
                                    cmds.push(Cmd::Goto { scene: i, view: None, focus: None });
                                }
                                if ui.small_button("↑").on_hover_text("Move up").clicked() && i > 0 {
                                    action = Some((i, -1));
                                }
                                if ui.small_button("↓").on_hover_text("Move down").clicked() && i + 1 < n {
                                    action = Some((i, 1));
                                }
                                if ui.small_button("★").on_hover_text("Make start scene").clicked() {
                                    action = Some((i, 0));
                                }
                                if n > 1 && ui.small_button("✕").on_hover_text("Remove scene").clicked() {
                                    editor.confirm_delete_scene = Some(i);
                                }
                            });
                        }
                        if let Some((i, d)) = action {
                            if d == 0 {
                                t.start_scene = Some(t.scenes[i].id.clone());
                            } else {
                                let j = (i as i32 + d) as usize;
                                t.scenes.swap(i, j);
                                if *scene_idx == i {
                                    *scene_idx = j;
                                } else if *scene_idx == j {
                                    *scene_idx = i;
                                }
                            }
                        }
                        if let Some(i) = editor.confirm_delete_scene {
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(format!("Remove '{}'?", t.scenes[i].id)).color(theme::DANGER));
                                if ui.button("Remove").clicked() {
                                    let id = t.scenes.remove(i).id;
                                    for s in &mut t.scenes {
                                        s.hotspots.retain(|h| !matches!(&h.action, Action::Goto { scene, .. } if *scene == id));
                                    }
                                    if t.start_scene.as_deref() == Some(id.as_str()) {
                                        t.start_scene = t.scenes.first().map(|s| s.id.clone());
                                    }
                                    editor.confirm_delete_scene = None;
                                    if *scene_idx >= i {
                                        let target = scene_idx.saturating_sub(1).min(t.scenes.len() - 1);
                                        cmds.push(Cmd::Goto { scene: target, view: None, focus: None });
                                    }
                                }
                                if ui.button("Cancel").clicked() {
                                    editor.confirm_delete_scene = None;
                                }
                            });
                        }
                        ui.add_space(6.0);
                        if icons::pill(ui, Icon::Plus, "Add panoramas…", false).clicked() {
                            add_scenes = true;
                        }
                    }
                    Tab::Tour => {
                        section(ui, "Tour");
                        ui.label("Title");
                        ui.add(egui::TextEdit::singleline(&mut t.title).desired_width(f32::INFINITY));
                        ui.label("Description");
                        ui.add(egui::TextEdit::multiline(&mut t.description).desired_rows(3).desired_width(f32::INFINITY));
                        let start = t.start_scene.clone().unwrap_or_default();
                        let start_title =
                            scene_ids.iter().find(|(i, _)| *i == start).map(|(_, t)| t.clone()).unwrap_or_else(|| "First scene".into());
                        egui::ComboBox::from_label("Start scene").width(180.0).selected_text(start_title).show_ui(ui, |ui| {
                            for (id, title) in &scene_ids {
                                if ui.selectable_label(start == *id, title).clicked() {
                                    t.start_scene = Some(id.clone());
                                }
                            }
                        });
                        let s = &mut t.settings;
                        section(ui, "Auto-rotation");
                        ui.checkbox(&mut s.autorotate.enabled, "Rotate when idle");
                        ui.add(egui::Slider::new(&mut s.autorotate.speed, -20.0..=20.0).text("°/s"));
                        ui.add(egui::Slider::new(&mut s.autorotate.delay, 0.0..=60.0).text("s idle"));
                        ui.checkbox(&mut s.autorotate.level_horizon, "Level the horizon");
                        section(ui, "Transitions");
                        egui::ComboBox::from_label("Style")
                            .width(180.0)
                            .selected_text(match s.transition.kind {
                                TransitionKind::None => "Cut",
                                TransitionKind::Fade => "Cross-fade",
                                TransitionKind::ZoomFade => "Zoom through",
                            })
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut s.transition.kind, TransitionKind::ZoomFade, "Zoom through");
                                ui.selectable_value(&mut s.transition.kind, TransitionKind::Fade, "Cross-fade");
                                ui.selectable_value(&mut s.transition.kind, TransitionKind::None, "Cut");
                            });
                        ui.add(egui::Slider::new(&mut s.transition.duration, 0.2..=3.0).text("s"));
                        section(ui, "Controls");
                        egui::ComboBox::from_label("Drag")
                            .width(180.0)
                            .selected_text(match s.controls.mode {
                                DragMode::Drag => "Grab & drag",
                                DragMode::Joystick => "Joystick",
                            })
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut s.controls.mode, DragMode::Drag, "Grab & drag");
                                ui.selectable_value(&mut s.controls.mode, DragMode::Joystick, "Joystick");
                            });
                        ui.add(egui::Slider::new(&mut s.controls.friction, 0.0..=1.0).text("friction"));
                        ui.checkbox(&mut s.controls.zoom_to_cursor, "Zoom towards the cursor");
                        ui.checkbox(&mut s.controls.invert, "Invert drag");
                        section(ui, "Appearance");
                        ui.checkbox(&mut s.compass, "Show compass");
                        ui.horizontal(|ui| {
                            ui.color_edit_button_srgb(&mut s.background);
                            ui.label("Background");
                        });
                        camera.autorotate = s.autorotate.clone();
                        camera.controls = s.controls.clone();
                        let warnings = t.warnings();
                        if !warnings.is_empty() {
                            section(ui, "Warnings");
                            for w in warnings {
                                ui.label(RichText::new(w).color(theme::ACCENT).size(12.0));
                            }
                        }
                    }
                }
            });
            editor.commit(t, ui.ctx());
        });
    // Shortcuts.
    // Check redo first: egui's logical matching would also accept Ctrl+Shift+Z as Ctrl+Z.
    let (undo, redo, save_key) = ctx.input_mut(|i| {
        let redo = i.consume_shortcut(&egui::KeyboardShortcut::new(egui::Modifiers::COMMAND | egui::Modifiers::SHIFT, Key::Z))
            || i.consume_shortcut(&egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, Key::Y));
        let undo = i.consume_shortcut(&egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, Key::Z));
        let save = i.consume_shortcut(&egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, Key::S));
        (undo, redo, save)
    });
    if let Some(t) = app.tour.as_mut() {
        if redo {
            app.editor.redo(&mut t.tour);
        } else if undo {
            app.editor.undo(&mut t.tour);
        }
    }
    if (save || save_key) && app.editor.dirty {
        app.cmds.push(Cmd::Save);
    }
    if add_scenes {
        crate::library::add_scenes_dialog(app, &ctx);
    }
}

/// Property editor for one hotspot. Returns true if it should be deleted.
fn hotspot_props(ui: &mut Ui, h: &mut Hotspot, scenes: &[(String, String)], current: nodal_core::View) -> bool {
    let mut delete = false;
    ui.label("Label");
    ui.add(egui::TextEdit::singleline(&mut h.label).hint_text("Shown on hover").desired_width(f32::INFINITY));
    if let HotspotKind::Point { yaw, pitch, icon } = &mut h.kind {
        ui.horizontal(|ui| {
            ui.label("Icon");
            egui::ComboBox::from_id_salt("icon").width(110.0).selected_text(icon.label()).show_ui(ui, |ui| {
                for s in IconStyle::ALL {
                    ui.selectable_value(icon, s, s.label());
                }
            });
        });
        ui.horizontal(|ui| {
            ui.add(egui::DragValue::new(yaw).range(-180.0..=180.0).speed(0.2).max_decimals(1).prefix("yaw ").suffix("°"));
            ui.add(egui::DragValue::new(pitch).range(-90.0..=90.0).speed(0.2).max_decimals(1).prefix("pitch ").suffix("°"));
        });
    } else if let HotspotKind::Polygon { points } = &h.kind {
        ui.label(
            RichText::new(format!("Area with {} corners — drag the corners to reshape.", points.len())).size(12.0).color(theme::MUTED),
        );
    }
    ui.add_space(4.0);
    let kind = match &h.action {
        Action::None => 0,
        Action::Goto { .. } => 1,
        Action::Info { .. } => 2,
        Action::Link { .. } => 3,
        Action::LookAt { .. } => 4,
    };
    let names = ["No action", "Go to scene", "Show info card", "Open web link", "Look at"];
    let mut new_kind = kind;
    egui::ComboBox::from_label("Action").width(190.0).selected_text(names[kind]).show_ui(ui, |ui| {
        for (i, n) in names.iter().enumerate() {
            ui.selectable_value(&mut new_kind, i, *n);
        }
    });
    if new_kind != kind {
        h.action = match new_kind {
            1 => Action::Goto { scene: scenes.first().map(|s| s.0.clone()).unwrap_or_default(), yaw: None, pitch: None, fov: None },
            2 => Action::Info { title: h.label.clone(), text: String::new() },
            3 => Action::Link { url: "https://".into() },
            4 => Action::LookAt { yaw: current.yaw, pitch: current.pitch, fov: Some(current.fov) },
            _ => Action::None,
        };
    }
    match &mut h.action {
        Action::Goto { scene, yaw, pitch, fov } => {
            let title = scenes.iter().find(|(i, _)| i == scene).map(|(_, t)| t.clone()).unwrap_or_else(|| scene.clone());
            egui::ComboBox::from_label("Target").width(190.0).selected_text(title).show_ui(ui, |ui| {
                for (id, t) in scenes {
                    ui.selectable_value(scene, id.clone(), t);
                }
            });
            let mut custom = yaw.is_some();
            if ui.checkbox(&mut custom, "Custom arrival direction").changed() {
                if custom {
                    *yaw = Some(0.0);
                    *pitch = Some(0.0);
                } else {
                    *yaw = None;
                    *pitch = None;
                    *fov = None;
                }
            }
            if let (Some(y), Some(p)) = (yaw.as_mut(), pitch.as_mut()) {
                ui.horizontal(|ui| {
                    ui.add(egui::DragValue::new(y).range(-180.0..=180.0).max_decimals(1).prefix("yaw ").suffix("°"));
                    ui.add(egui::DragValue::new(p).range(-90.0..=90.0).max_decimals(1).prefix("pitch ").suffix("°"));
                });
            }
        }
        Action::Info { title, text } => {
            ui.label("Card title");
            ui.add(egui::TextEdit::singleline(title).desired_width(f32::INFINITY));
            ui.label("Card text");
            ui.add(egui::TextEdit::multiline(text).desired_rows(4).desired_width(f32::INFINITY));
        }
        Action::Link { url } => {
            ui.add(egui::TextEdit::singleline(url).desired_width(f32::INFINITY));
        }
        Action::LookAt { yaw, pitch, fov } => {
            ui.label(RichText::new(format!("yaw {yaw:.0}°, pitch {pitch:.0}°")).size(12.0).color(theme::MUTED));
            if ui.button("Use current view").clicked() {
                *yaw = current.yaw;
                *pitch = current.pitch;
                *fov = Some(current.fov);
            }
        }
        Action::None => {}
    }
    ui.add_space(8.0);
    if ui.add(egui::Button::new(RichText::new("Delete hotspot").color(theme::DANGER))).clicked() {
        delete = true;
    }
    delete
}

fn dir_to_deg(proj: &Projector, map: &ScreenMap, p: egui::Pos2) -> Option<[f32; 2]> {
    let d = proj.screen_to_dir(map.to_px(p))?;
    let (y, pt) = yaw_pitch_from_dir(d);
    Some([y / DEG, pt / DEG])
}

/// Handles editor interactions in the viewport. Returns true if the pointer was consumed.
pub fn viewport_input(app: &mut NodalApp, ctx: &egui::Context, resp: &egui::Response, proj: &Projector, map: &ScreenMap) -> bool {
    let NodalApp { tour, editor, scene: scene_idx, .. } = app;
    let Some(lt) = tour.as_mut() else { return false };
    let Some(scene) = lt.tour.scenes.get_mut(*scene_idx) else { return false };
    let hover = resp.hover_pos();

    if !ctx.egui_wants_keyboard_input() {
        if let Some(i) = editor.selected {
            if ctx.input(|inp| inp.key_pressed(Key::Delete) || inp.key_pressed(Key::Backspace)) && i < scene.hotspots.len() {
                scene.hotspots.remove(i);
                editor.selected = None;
                return true;
            }
        }
    }

    match &mut editor.placing {
        Some(Placing::Point) => {
            ctx.set_cursor_icon(CursorIcon::Crosshair);
            if resp.clicked() {
                if let Some([y, p]) = hover.and_then(|h| dir_to_deg(proj, map, h)) {
                    let id = scene.unique_hotspot_id("hotspot");
                    let mut h = new_point(id, y, p);
                    if let Some(other) = lt.tour.scenes.iter().find(|s| s.id != lt.tour.scenes[*scene_idx].id) {
                        h.action = Action::Goto { scene: other.id.clone(), yaw: None, pitch: None, fov: None };
                        h.label = other.title.clone();
                    }
                    let scene = &mut lt.tour.scenes[*scene_idx];
                    scene.hotspots.push(h);
                    editor.selected = Some(scene.hotspots.len() - 1);
                    editor.placing = None;
                }
                return true;
            }
            return false;
        }
        Some(Placing::Polygon(pts)) => {
            ctx.set_cursor_icon(CursorIcon::Crosshair);
            let finish = (resp.double_clicked() || ctx.input(|i| i.key_pressed(Key::Enter))) && pts.len() >= 3;
            if finish {
                // A double-click also produced a click on the same spot; drop that duplicate.
                if resp.double_clicked() && pts.len() > 3 {
                    pts.pop();
                }
                let id = scene.unique_hotspot_id("area");
                scene.hotspots.push(Hotspot {
                    id,
                    kind: HotspotKind::Polygon { points: pts.clone() },
                    label: String::new(),
                    action: Action::None,
                });
                editor.selected = Some(scene.hotspots.len() - 1);
                editor.placing = None;
                return true;
            }
            if resp.clicked() {
                if let Some(p) = hover.and_then(|h| dir_to_deg(proj, map, h)) {
                    pts.push(p);
                }
                return true;
            }
            return false;
        }
        None => {}
    }

    // Dragging hotspots and polygon corners.
    if resp.drag_started() {
        if let Some(p) = resp.interact_pointer_pos() {
            let mut target = None;
            if let Some(sel) = editor.selected {
                if let Some(Hotspot { kind: HotspotKind::Polygon { points }, .. }) = scene.hotspots.get(sel) {
                    for (vi, v) in points.iter().enumerate() {
                        if let Some(px) = proj.dir_to_screen(hotspots::dir_of(v[0], v[1])) {
                            if map.to_pos(px).distance(p) < 10.0 {
                                target = Some(DragTarget::Vertex(sel, vi));
                            }
                        }
                    }
                }
            }
            if target.is_none() {
                if let Some(i) = hotspots::hit_test(proj, map, scene, p) {
                    if matches!(scene.hotspots[i].kind, HotspotKind::Point { .. }) {
                        target = Some(DragTarget::Point(i));
                        editor.selected = Some(i);
                    }
                }
            }
            editor.drag = target;
        }
    }
    if let Some(target) = editor.drag {
        if resp.dragged() {
            if let Some([y, pt]) = resp.interact_pointer_pos().and_then(|p| dir_to_deg(proj, map, p)) {
                match target {
                    DragTarget::Point(i) => {
                        if let Some(HotspotKind::Point { yaw, pitch, .. }) = scene.hotspots.get_mut(i).map(|h| &mut h.kind) {
                            *yaw = y;
                            *pitch = pt;
                        }
                    }
                    DragTarget::Vertex(i, vi) => {
                        if let Some(HotspotKind::Polygon { points }) = scene.hotspots.get_mut(i).map(|h| &mut h.kind) {
                            if let Some(v) = points.get_mut(vi) {
                                *v = [y, pt];
                            }
                        }
                    }
                }
            }
            ctx.set_cursor_icon(CursorIcon::Grabbing);
        }
        if resp.drag_stopped() {
            editor.drag = None;
        }
        return true;
    }
    if resp.clicked() {
        editor.selected = hover.and_then(|p| hotspots::hit_test(proj, map, scene, p));
        return true;
    }
    false
}

pub fn viewport_overlay(app: &mut NodalApp, ui: &mut Ui, proj: &Projector, map: &ScreenMap, resp: &egui::Response) {
    if let Some(Placing::Polygon(pts)) = &app.editor.placing {
        let screen: Vec<egui::Pos2> =
            pts.iter().filter_map(|p| proj.dir_to_screen(hotspots::dir_of(p[0], p[1]))).map(|px| map.to_pos(px)).collect();
        hotspots::draw_placement_preview(ui.painter(), &screen, resp.hover_pos());
    }
    if app.editor.placing == Some(Placing::Point) {
        if let Some(p) = resp.hover_pos() {
            ui.painter().circle_stroke(p, hotspots::POINT_RADIUS, egui::Stroke::new(2.0, theme::ACCENT));
            ui.painter().circle_filled(p, 3.0, Color32::WHITE);
        }
    }
}

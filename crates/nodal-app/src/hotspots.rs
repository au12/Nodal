//! Hotspot drawing and hit-testing in screen space.

use crate::icons::{self, Icon};
use crate::theme;
use egui::{pos2, Align2, Color32, Mesh, Painter, Pos2, Rect, Shape, Stroke, Vec2};
use glam::{Vec2 as GVec2, Vec3};
use nodal_core::math::{dir_from_yaw_pitch, slerp, DEG};
use nodal_core::tour::{Action, Hotspot, HotspotKind, IconStyle, Scene};
use nodal_core::{ProjMode, Projector};

/// Converts between projector pixels (physical, viewport-relative) and egui points.
#[derive(Clone, Copy)]
pub struct ScreenMap {
    pub rect: Rect,
    pub ppp: f32,
}

impl ScreenMap {
    pub fn to_pos(self, px: GVec2) -> Pos2 {
        pos2(self.rect.min.x + px.x / self.ppp, self.rect.min.y + px.y / self.ppp)
    }
    pub fn to_px(self, p: Pos2) -> GVec2 {
        GVec2::new((p.x - self.rect.min.x) * self.ppp, (p.y - self.rect.min.y) * self.ppp)
    }
}

pub fn dir_of(yaw: f32, pitch: f32) -> Vec3 {
    dir_from_yaw_pitch(yaw * DEG, pitch * DEG)
}

pub const POINT_RADIUS: f32 = 19.0;

/// Screen-space outline of a polygon hotspot (great-circle edges, clipped to the visible cone).
pub fn polygon_outline(proj: &Projector, map: &ScreenMap, points: &[[f32; 2]]) -> Vec<Pos2> {
    if points.len() < 3 {
        return vec![];
    }
    let dirs: Vec<Vec3> = points.iter().map(|p| dir_of(p[0], p[1])).collect();
    let mut ring = vec![];
    for i in 0..dirs.len() {
        let (a, b) = (dirs[i], dirs[(i + 1) % dirs.len()]);
        let segs = ((a.angle_between(b) / (1.5 * DEG)).ceil() as usize).clamp(1, 120);
        for s in 0..segs {
            ring.push(slerp(a, b, s as f32 / segs as f32));
        }
    }
    if proj.mode != ProjMode::Equirect {
        // Sutherland–Hodgman against the plane bounding the visible cone.
        let fwd = proj.center_dir();
        let cone = (proj.visible_cone() - 4.0 * DEG).min(178.0 * DEG);
        let c = cone.cos();
        let inside = |v: &Vec3| v.dot(fwd) >= c;
        let mut out = vec![];
        for i in 0..ring.len() {
            let (a, b) = (ring[i], ring[(i + 1) % ring.len()]);
            let (ia, ib) = (inside(&a), inside(&b));
            if ia {
                out.push(a);
            }
            if ia != ib {
                let da = a.dot(fwd) - c;
                let db = b.dot(fwd) - c;
                let t = da / (da - db);
                out.push(a.lerp(b, t).normalize());
            }
        }
        ring = out;
    }
    let pts: Vec<Pos2> = ring.iter().filter_map(|d| proj.dir_to_screen(*d)).map(|p| map.to_pos(p)).collect();
    if proj.mode == ProjMode::Equirect {
        // Skip polygons that straddle the ±180° seam of the flat view.
        let w = map.rect.width();
        if pts.windows(2).any(|w2| (w2[0].x - w2[1].x).abs() > w * 0.5) {
            return vec![];
        }
    }
    pts
}

pub fn point_in_polygon(p: Pos2, poly: &[Pos2]) -> bool {
    let mut inside = false;
    let n = poly.len();
    let mut j = n.wrapping_sub(1);
    for i in 0..n {
        let (a, b) = (poly[i], poly[j]);
        if (a.y > p.y) != (b.y > p.y) && p.x < (b.x - a.x) * (p.y - a.y) / (b.y - a.y) + a.x {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// Ear-clipping triangulation of a simple polygon (handles concave outlines).
pub fn triangulate(pts: &[Pos2]) -> Vec<[u32; 3]> {
    let n = pts.len();
    if n < 3 {
        return vec![];
    }
    let area: f32 = (0..n)
        .map(|i| {
            let (a, b) = (pts[i], pts[(i + 1) % n]);
            a.x * b.y - b.x * a.y
        })
        .sum();
    let mut idx: Vec<usize> = if area > 0.0 { (0..n).collect() } else { (0..n).rev().collect() };
    let cross = |a: Pos2, b: Pos2, c: Pos2| (b.x - a.x) * (c.y - a.y) - (b.y - a.y) * (c.x - a.x);
    let in_tri = |p: Pos2, a: Pos2, b: Pos2, c: Pos2| cross(a, b, p) >= 0.0 && cross(b, c, p) >= 0.0 && cross(c, a, p) >= 0.0;
    let mut tris = vec![];
    let mut guard = 0;
    while idx.len() > 3 && guard < n * n {
        guard += 1;
        let m = idx.len();
        let mut clipped = false;
        for i in 0..m {
            let (ia, ib, ic) = (idx[(i + m - 1) % m], idx[i], idx[(i + 1) % m]);
            let (a, b, c) = (pts[ia], pts[ib], pts[ic]);
            if cross(a, b, c) <= 0.0 {
                continue;
            }
            if idx.iter().any(|&k| k != ia && k != ib && k != ic && in_tri(pts[k], a, b, c)) {
                continue;
            }
            tris.push([ia as u32, ib as u32, ic as u32]);
            idx.remove(i);
            clipped = true;
            break;
        }
        if !clipped {
            break; // degenerate (self-intersecting) — stop rather than loop
        }
    }
    if idx.len() == 3 {
        tris.push([idx[0] as u32, idx[1] as u32, idx[2] as u32]);
    }
    tris
}

fn fill_polygon(painter: &Painter, pts: &[Pos2], color: Color32) {
    let tris = triangulate(pts);
    if tris.is_empty() {
        return;
    }
    let mut mesh = Mesh::default();
    for p in pts {
        mesh.colored_vertex(*p, color);
    }
    for t in tris {
        mesh.add_triangle(t[0], t[1], t[2]);
    }
    painter.add(Shape::mesh(mesh));
}

pub struct DrawState {
    pub hovered: Option<usize>,
    pub selected: Option<usize>,
    pub editing: bool,
    pub show_labels: bool,
    pub time: f64,
}

/// Screen position of a point hotspot, if visible.
pub fn point_pos(proj: &Projector, map: &ScreenMap, h: &Hotspot) -> Option<Pos2> {
    let HotspotKind::Point { yaw, pitch, .. } = h.kind else { return None };
    let p = proj.dir_to_screen(dir_of(yaw, pitch))?;
    let pos = map.to_pos(p);
    map.rect.expand(40.0).contains(pos).then_some(pos)
}

/// Topmost hotspot under `pos`.
pub fn hit_test(proj: &Projector, map: &ScreenMap, scene: &Scene, pos: Pos2) -> Option<usize> {
    for (i, h) in scene.hotspots.iter().enumerate().rev() {
        match &h.kind {
            HotspotKind::Point { .. } => {
                if let Some(c) = point_pos(proj, map, h) {
                    if c.distance(pos) <= POINT_RADIUS + 4.0 {
                        return Some(i);
                    }
                }
            }
            HotspotKind::Polygon { points } => {
                let poly = polygon_outline(proj, map, points);
                if poly.len() >= 3 && point_in_polygon(pos, &poly) {
                    return Some(i);
                }
            }
        }
    }
    None
}

fn icon_for(h: &Hotspot) -> (IconStyle, Icon) {
    let style = match &h.kind {
        HotspotKind::Point { icon, .. } => *icon,
        _ => IconStyle::Dot,
    };
    let icon = match style {
        IconStyle::Arrow => Icon::Arrow,
        IconStyle::Info => Icon::Info,
        IconStyle::Pin => Icon::Pin,
        IconStyle::Dot => Icon::Target,
    };
    (style, icon)
}

pub fn draw(painter: &Painter, proj: &Projector, map: &ScreenMap, scene: &Scene, st: &DrawState) {
    let painter = painter.with_clip_rect(map.rect);
    // Polygons first, so point icons sit on top.
    for (i, h) in scene.hotspots.iter().enumerate() {
        let HotspotKind::Polygon { points } = &h.kind else { continue };
        let poly = polygon_outline(proj, map, points);
        if poly.len() < 3 {
            continue;
        }
        let hot = st.hovered == Some(i) || st.selected == Some(i);
        let fill = if hot {
            Color32::from_rgba_unmultiplied(255, 255, 255, 46)
        } else if st.editing {
            Color32::from_rgba_unmultiplied(255, 181, 71, 26)
        } else {
            Color32::from_rgba_unmultiplied(255, 255, 255, 10)
        };
        fill_polygon(&painter, &poly, fill);
        let stroke = if st.selected == Some(i) {
            Stroke::new(2.0, theme::ACCENT)
        } else if hot || st.editing {
            Stroke::new(1.5, Color32::from_rgba_unmultiplied(255, 255, 255, 190))
        } else {
            Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 255, 255, 60))
        };
        let mut closed = poly.clone();
        closed.push(poly[0]);
        painter.add(Shape::line(closed, stroke));
        if st.selected == Some(i) {
            for p in points {
                if let Some(px) = proj.dir_to_screen(dir_of(p[0], p[1])) {
                    let c = map.to_pos(px);
                    painter.circle_filled(c, 5.0, theme::ACCENT);
                    painter.circle_stroke(c, 5.0, Stroke::new(1.5, Color32::BLACK));
                }
            }
        }
        if hot && !h.label.is_empty() {
            let (yaw, pitch) = h.kind.anchor();
            if let Some(px) = proj.dir_to_screen(dir_of(yaw, pitch)) {
                label(&painter, map.to_pos(px), &h.label, 0.0);
            }
        }
    }
    for (i, h) in scene.hotspots.iter().enumerate() {
        let Some(c) = point_pos(proj, map, h) else { continue };
        let hot = st.hovered == Some(i);
        let sel = st.selected == Some(i);
        let t = painter.ctx().animate_bool(egui::Id::unique(("hs", i, &h.id)), hot || sel);
        let (style, icon) = icon_for(h);
        let r = POINT_RADIUS * (1.0 + 0.12 * t);
        // Soft pulse ring to make hotspots discoverable.
        let phase = ((st.time * 0.8 + i as f64 * 0.37).fract()) as f32;
        let ring_a = ((1.0 - phase) * 70.0 * (1.0 - t)) as u8;
        painter.circle_stroke(c, r + 3.0 + phase * 12.0, Stroke::new(2.0, Color32::from_rgba_unmultiplied(255, 255, 255, ring_a)));
        painter.circle_filled(c + Vec2::new(0.0, 2.0), r + 1.0, Color32::from_black_alpha(70));
        let bg = if style == IconStyle::Arrow {
            icons::lerp_color(Color32::from_rgba_unmultiplied(255, 255, 255, 225), theme::ACCENT, t)
        } else {
            icons::lerp_color(Color32::from_rgba_unmultiplied(20, 22, 27, 215), theme::ACCENT, t)
        };
        painter.circle_filled(c, r, bg);
        let fg = if style == IconStyle::Arrow || t > 0.5 { Color32::from_rgb(20, 20, 24) } else { Color32::WHITE };
        if style == IconStyle::Arrow {
            // Chevron pointing "forward" (into the scene).
            let s = r / 19.0;
            painter.add(Shape::line(
                vec![c + Vec2::new(-7.0 * s, 3.5 * s), c + Vec2::new(0.0, -4.0 * s), c + Vec2::new(7.0 * s, 3.5 * s)],
                Stroke::new(3.0 * s, fg),
            ));
        } else {
            icons::paint(&painter, Rect::from_center_size(c, Vec2::splat(r * 1.15)), icon, fg);
        }
        if sel {
            painter.circle_stroke(c, r + 3.0, Stroke::new(2.0, theme::ACCENT));
        }
        if (hot || sel || st.show_labels) && !h.label.is_empty() {
            label(&painter, c, &h.label, r);
        }
    }
}

fn label(painter: &Painter, anchor: Pos2, text: &str, offset: f32) {
    let galley = painter.layout_no_wrap(text.to_string(), theme::heading_font(13.5), Color32::WHITE);
    let size = galley.size() + Vec2::new(20.0, 10.0);
    let center = anchor - Vec2::new(0.0, offset + 12.0 + size.y / 2.0);
    let rect = Rect::from_center_size(center, size);
    painter.rect_filled(rect.translate(Vec2::new(0.0, 2.0)), 10.0, Color32::from_black_alpha(60));
    painter.rect_filled(rect, 10.0, theme::glass());
    painter.galley(rect.center() - galley.size() / 2.0, galley, Color32::WHITE);
}

/// Short human description of an action (editor list, tooltips).
pub fn describe(action: &Action, scene_title: impl Fn(&str) -> String) -> String {
    match action {
        Action::None => "No action".into(),
        Action::Goto { scene, .. } => format!("Go to {}", scene_title(scene)),
        Action::Info { title, .. } => format!("Info: {}", if title.is_empty() { "untitled" } else { title }),
        Action::Link { url } => format!("Link: {url}"),
        Action::LookAt { .. } => "Look at".into(),
    }
}

pub fn draw_placement_preview(painter: &Painter, pts: &[Pos2], cursor: Option<Pos2>) {
    let mut all = pts.to_vec();
    if let Some(c) = cursor {
        all.push(c);
    }
    if all.len() >= 2 {
        painter.add(Shape::line(all.clone(), Stroke::new(2.0, theme::ACCENT)));
    }
    for p in pts {
        painter.circle_filled(*p, 4.5, theme::ACCENT);
    }
    if let Some(c) = cursor {
        painter.text(
            c + Vec2::new(14.0, -14.0),
            Align2::LEFT_BOTTOM,
            if pts.len() >= 3 { "Enter / double-click to finish" } else { "Click to add points" },
            theme::body_font(12.0),
            Color32::WHITE,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triangulates_concave_l_shape() {
        let pts = [pos2(0.0, 0.0), pos2(2.0, 0.0), pos2(2.0, 1.0), pos2(1.0, 1.0), pos2(1.0, 2.0), pos2(0.0, 2.0)];
        let tris = triangulate(&pts);
        assert_eq!(tris.len(), 4);
        let area: f32 = tris
            .iter()
            .map(|t| {
                let (a, b, c) = (pts[t[0] as usize], pts[t[1] as usize], pts[t[2] as usize]);
                ((b.x - a.x) * (c.y - a.y) - (b.y - a.y) * (c.x - a.x)).abs() / 2.0
            })
            .sum();
        assert!((area - 3.0).abs() < 1e-4);
    }

    #[test]
    fn point_in_poly() {
        let sq = [pos2(0.0, 0.0), pos2(10.0, 0.0), pos2(10.0, 10.0), pos2(0.0, 10.0)];
        assert!(point_in_polygon(pos2(5.0, 5.0), &sq));
        assert!(!point_in_polygon(pos2(15.0, 5.0), &sq));
    }
}

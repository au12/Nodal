//! The tour file format (`tour.json`).
//!
//! A tour is a list of scenes; each scene is one tiled panorama with a start view,
//! limits and hotspots. All paths are relative to the directory containing `tour.json`.

use crate::pyramid::Pyramid;
use crate::view::{View, ViewLimits};
use serde::{Deserialize, Serialize};

pub const FORMAT_VERSION: u32 = 1;

fn v1() -> u32 {
    FORMAT_VERSION
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Tour {
    #[serde(default = "v1")]
    pub version: u32,
    #[serde(default)]
    pub title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_scene: Option<String>,
    #[serde(default)]
    pub settings: TourSettings,
    pub scenes: Vec<Scene>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TourSettings {
    #[serde(default)]
    pub autorotate: Autorotate,
    #[serde(default)]
    pub transition: Transition,
    #[serde(default)]
    pub controls: Controls,
    /// Colour shown where there is no image data.
    #[serde(default = "default_bg")]
    pub background: [u8; 3],
    /// Show a compass/radar overlay.
    #[serde(default = "yes")]
    pub compass: bool,
}

fn default_bg() -> [u8; 3] {
    [12, 13, 16]
}
fn yes() -> bool {
    true
}

impl Default for TourSettings {
    fn default() -> Self {
        Self {
            autorotate: Autorotate::default(),
            transition: Transition::default(),
            controls: Controls::default(),
            background: default_bg(),
            compass: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Autorotate {
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Degrees per second (negative turns left).
    #[serde(default = "d_ar_speed")]
    pub speed: f32,
    /// Seconds of inactivity before rotation starts.
    #[serde(default = "d_ar_delay")]
    pub delay: f32,
    /// Ease pitch back to the horizon while rotating.
    #[serde(default = "yes")]
    pub level_horizon: bool,
    /// Rotation winds down after this many seconds without input, so an idle window stops
    /// drawing (and stops using power). 0 = keep rotating forever (kiosks).
    #[serde(default = "d_ar_max")]
    pub max_seconds: f32,
}

fn d_ar_max() -> f32 {
    180.0
}

fn d_ar_speed() -> f32 {
    4.0
}
fn d_ar_delay() -> f32 {
    6.0
}

impl Default for Autorotate {
    fn default() -> Self {
        Self { enabled: true, speed: d_ar_speed(), delay: d_ar_delay(), level_horizon: true, max_seconds: d_ar_max() }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TransitionKind {
    None,
    Fade,
    /// Zoom towards the hotspot while cross-fading.
    #[default]
    ZoomFade,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Transition {
    #[serde(default)]
    pub kind: TransitionKind,
    /// Seconds.
    #[serde(default = "d_tr_dur")]
    pub duration: f32,
}

fn d_tr_dur() -> f32 {
    0.9
}

impl Default for Transition {
    fn default() -> Self {
        Self { kind: TransitionKind::ZoomFade, duration: d_tr_dur() }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DragMode {
    /// The image follows the pointer ("grab and drag").
    #[default]
    Drag,
    /// Pointer offset sets the rotation speed (QTVR style).
    Joystick,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Controls {
    #[serde(default)]
    pub mode: DragMode,
    /// 0..1, how quickly inertia decays (higher stops sooner).
    #[serde(default = "d_friction")]
    pub friction: f32,
    /// Keep the point under the cursor fixed while zooming with the wheel.
    #[serde(default = "yes")]
    pub zoom_to_cursor: bool,
    #[serde(default)]
    pub invert: bool,
}

fn d_friction() -> f32 {
    0.55
}

impl Default for Controls {
    fn default() -> Self {
        Self { mode: DragMode::Drag, friction: d_friction(), zoom_to_cursor: true, invert: false }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Scene {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    pub pyramid: Pyramid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumb: Option<String>,
    /// Start view.
    #[serde(default)]
    pub view: View,
    #[serde(default)]
    pub limits: ViewLimits,
    /// Yaw (degrees) that points north, for the compass.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub north: f32,
    #[serde(default)]
    pub hotspots: Vec<Hotspot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<SourceInfo>,
}

fn is_zero(v: &f32) -> bool {
    *v == 0.0
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SourceInfo {
    pub width: u32,
    pub height: u32,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub file: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Hotspot {
    pub id: String,
    #[serde(flatten)]
    pub kind: HotspotKind,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub label: String,
    #[serde(default)]
    pub action: Action,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "shape", rename_all = "snake_case")]
pub enum HotspotKind {
    Point {
        yaw: f32,
        pitch: f32,
        #[serde(default)]
        icon: IconStyle,
    },
    /// Closed polygon of `[yaw, pitch]` vertices in degrees; edges follow great circles.
    Polygon { points: Vec<[f32; 2]> },
}

impl HotspotKind {
    /// Representative direction (yaw, pitch in degrees).
    pub fn anchor(&self) -> (f32, f32) {
        match self {
            HotspotKind::Point { yaw, pitch, .. } => (*yaw, *pitch),
            HotspotKind::Polygon { points } => {
                use crate::math::{dir_from_yaw_pitch, yaw_pitch_from_dir, DEG};
                let sum = points.iter().map(|p| dir_from_yaw_pitch(p[0] * DEG, p[1] * DEG)).fold(glam::Vec3::ZERO, |a, b| a + b);
                let (y, p) = yaw_pitch_from_dir(sum);
                (y / DEG, p / DEG)
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum IconStyle {
    /// Navigation chevron, the default for scene links.
    #[default]
    Arrow,
    Info,
    Pin,
    Dot,
}

impl IconStyle {
    pub const ALL: [IconStyle; 4] = [IconStyle::Arrow, IconStyle::Info, IconStyle::Pin, IconStyle::Dot];
    pub fn label(self) -> &'static str {
        match self {
            IconStyle::Arrow => "Arrow",
            IconStyle::Info => "Info",
            IconStyle::Pin => "Pin",
            IconStyle::Dot => "Dot",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Action {
    #[default]
    None,
    /// Go to another scene. Unset view fields use the target scene's start view.
    Goto {
        scene: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        yaw: Option<f32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pitch: Option<f32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fov: Option<f32>,
    },
    /// Show an information card.
    Info {
        #[serde(default)]
        title: String,
        #[serde(default)]
        text: String,
    },
    /// Open a web link.
    Link { url: String },
    /// Animate the camera.
    LookAt {
        yaw: f32,
        pitch: f32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fov: Option<f32>,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum TourError {
    #[error("tour.json is not valid: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Invalid(String),
}

impl Tour {
    pub fn from_json(s: &str) -> Result<Tour, TourError> {
        let tour: Tour = serde_json::from_str(s)?;
        tour.validate()?;
        Ok(tour)
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("tour serialises")
    }

    pub fn validate(&self) -> Result<(), TourError> {
        if self.scenes.is_empty() {
            return Err(TourError::Invalid("tour has no scenes".into()));
        }
        let mut ids = std::collections::HashSet::new();
        for s in &self.scenes {
            if !ids.insert(s.id.as_str()) {
                return Err(TourError::Invalid(format!("duplicate scene id '{}'", s.id)));
            }
            if s.pyramid.levels == 0 || s.pyramid.levels > 12 || s.pyramid.tile_size == 0 {
                return Err(TourError::Invalid(format!("scene '{}' has an invalid pyramid", s.id)));
            }
        }
        if let Some(start) = &self.start_scene {
            if !ids.contains(start.as_str()) {
                return Err(TourError::Invalid(format!("start scene '{start}' does not exist")));
            }
        }
        Ok(())
    }

    /// Problems that do not prevent loading (dangling links, empty labels…).
    pub fn warnings(&self) -> Vec<String> {
        let ids: std::collections::HashSet<_> = self.scenes.iter().map(|s| s.id.as_str()).collect();
        let mut out = vec![];
        for s in &self.scenes {
            for h in &s.hotspots {
                if let Action::Goto { scene, .. } = &h.action {
                    if !ids.contains(scene.as_str()) {
                        out.push(format!("hotspot '{}' in '{}' links to missing scene '{}'", h.id, s.id, scene));
                    }
                }
            }
        }
        out
    }

    pub fn scene(&self, id: &str) -> Option<&Scene> {
        self.scenes.iter().find(|s| s.id == id)
    }

    pub fn scene_index(&self, id: &str) -> Option<usize> {
        self.scenes.iter().position(|s| s.id == id)
    }

    pub fn start_index(&self) -> usize {
        self.start_scene.as_deref().and_then(|id| self.scene_index(id)).unwrap_or(0)
    }

    /// A short, URL-safe id that is unique within this tour.
    pub fn unique_scene_id(&self, wanted: &str) -> String {
        unique_id(wanted, |c| self.scene(c).is_some())
    }
}

impl Scene {
    pub fn unique_hotspot_id(&self, wanted: &str) -> String {
        unique_id(wanted, |c| self.hotspots.iter().any(|h| h.id == c))
    }
}

/// Lowercase ASCII slug: `"Piazza Bologni (2)"` → `"piazza-bologni-2"`.
pub fn slugify(s: &str) -> String {
    let mut out = String::new();
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        out.push_str("scene");
    }
    out.truncate(48);
    out
}

fn unique_id(wanted: &str, taken: impl Fn(&str) -> bool) -> String {
    let base = slugify(wanted);
    if !taken(&base) {
        return base;
    }
    (2..).map(|n| format!("{base}-{n}")).find(|c| !taken(c)).unwrap()
}

/// Joins a relative path onto a base directory or URL. Absolute URLs and paths are returned unchanged.
pub fn join_url(base: &str, rel: &str) -> String {
    if rel.contains("://") || rel.starts_with("data:") {
        return rel.to_string();
    }
    if let Some(stripped) = rel.strip_prefix('/') {
        // Root-relative: keep the scheme+host of an http base.
        if let Some(idx) = base.find("://") {
            let host_end = base[idx + 3..].find('/').map(|i| idx + 3 + i).unwrap_or(base.len());
            return format!("{}/{}", &base[..host_end], stripped);
        }
        return rel.to_string();
    }
    if base.is_empty() {
        return rel.to_string();
    }
    if base.ends_with('/') {
        format!("{base}{rel}")
    } else {
        format!("{base}/{rel}")
    }
}

/// Directory part of a path or URL (`"a/b/tour.json"` → `"a/b"`).
pub fn parent_url(path: &str) -> String {
    match path.rfind('/') {
        Some(i) => path[..i].to_string(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
      "title": "Demo",
      "start_scene": "b",
      "scenes": [
        {"id": "a", "pyramid": {"tile_size": 326, "levels": 3, "url": "a/{f}/{l}/{y}_{x}.jpg"},
         "hotspots": [
           {"id": "h1", "shape": "point", "yaw": 10, "pitch": -5, "label": "Go", "action": {"type": "goto", "scene": "b", "yaw": 30}},
           {"id": "h2", "shape": "polygon", "points": [[0,0],[10,0],[10,10]], "action": {"type": "info", "title": "T", "text": "x"}}
         ]},
        {"id": "b", "pyramid": {"tile_size": 326, "levels": 3, "url": "b/{f}/{l}/{y}_{x}.jpg"},
         "view": {"yaw": 45, "fov": 80, "projection": {"mode": "pannini", "distortion": 1}}}
      ]
    }"#;

    #[test]
    fn parse_and_roundtrip() {
        let t = Tour::from_json(SAMPLE).unwrap();
        assert_eq!(t.start_index(), 1);
        assert_eq!(t.scenes[0].pyramid.border, 1);
        assert!(matches!(t.scenes[0].hotspots[0].kind, HotspotKind::Point { icon: IconStyle::Arrow, .. }));
        assert!(matches!(t.scenes[0].hotspots[0].action, Action::Goto { yaw: Some(30.0), pitch: None, .. }));
        assert_eq!(t.scenes[1].view.projection.mode, crate::ProjMode::Pannini);
        let again = Tour::from_json(&t.to_json()).unwrap();
        assert_eq!(t, again);
        assert!(t.warnings().is_empty());
    }

    #[test]
    fn rejects_bad_tours() {
        assert!(Tour::from_json(r#"{"scenes": []}"#).is_err());
        let dup = SAMPLE.replace("\"id\": \"b\", \"pyramid\"", "\"id\": \"a\", \"pyramid\"");
        assert!(Tour::from_json(&dup).is_err());
    }

    #[test]
    fn urls() {
        assert_eq!(join_url("tours/x", "a/1.jpg"), "tours/x/a/1.jpg");
        assert_eq!(join_url("http://h:8080/tours/x/", "a.jpg"), "http://h:8080/tours/x/a.jpg");
        assert_eq!(join_url("http://h:8080/tours/x", "/api/t"), "http://h:8080/api/t");
        assert_eq!(join_url("x", "https://cdn/a.jpg"), "https://cdn/a.jpg");
        assert_eq!(parent_url("a/b/tour.json"), "a/b");
        assert_eq!(slugify("Piazza Bologni (2)"), "piazza-bologni-2");
    }

    #[test]
    fn unique_ids() {
        let t = Tour::from_json(SAMPLE).unwrap();
        assert_eq!(t.unique_scene_id("A"), "a-2");
        assert_eq!(t.unique_scene_id("Venice"), "venice");
    }
}

//! Ground-truth benchmark suite: synthetic photo sets rendered from a known panorama with known
//! lenses and orientations, stitched, and scored by **true misregistration**: for scene directions
//! that two or more photos see, how far apart the estimated model places the same point, in
//! source pixels. That is the ghosting a viewer would see; unlike control-point RMS, the
//! optimiser cannot flatter it.

use crate::align::{AlignOptions, RescueHandle};
use crate::features::DetectOptions;
use crate::lens::{Lens, LensKind, V3};
use crate::project::{rotation_from_ypr, Project, WarpField};
use crate::synth::{procedural_pano, render_shot, ShotOptions};
use crate::{Progress, Source};
use nalgebra::Rotation3;
use rayon::prelude::*;
use std::sync::Arc;
use std::time::Instant;

/// (rms, median, p95, max) in source pixels.
pub type Spread = (f64, f64, f64, f64);

/// Builds the learned rescue matcher for a set of photos.
pub type RescueFactory<'a> = &'a dyn Fn(&[Source]) -> RescueHandle;

/// One synthetic set.
#[derive(Clone, Debug)]
pub struct Case {
    pub name: &'static str,
    pub description: &'static str,
    pub lens: Lens,
    /// (yaw, pitch, roll) in degrees.
    pub poses: Vec<(f64, f64, f64)>,
    /// Record the focal length in EXIF, as cameras do (partial panoramas need it).
    pub exif_focal: bool,
}

/// Scores for one set.
#[derive(Clone, Debug, Default)]
pub struct CaseResult {
    pub name: String,
    pub description: String,
    pub photos: usize,
    /// Placed from content.
    pub placed: usize,
    /// Placed from the shooting pattern.
    pub estimated: usize,
    pub learned_pairs: usize,
    pub align_seconds: f32,
    /// True misregistration with local warps, over every overlap.
    pub all: Spread,
    /// The same in the outer part of the frames, where seams usually run.
    pub edges: Spread,
    pub lens: String,
    pub true_lens: String,
}

fn fisheye(kind: LensKind) -> Lens {
    let mut l = Lens::new(kind, 110.0, 1000, 1500);
    l.a = 0.01;
    l.b = -0.03;
    l
}

fn fisheye_poses() -> Vec<(f64, f64, f64)> {
    vec![
        (0.0, 2.0, 1.0),
        (61.0, -1.0, -2.0),
        (119.0, 3.0, 0.5),
        (182.0, 0.0, 1.5),
        (240.0, -2.0, -1.0),
        (301.0, 1.0, 0.0),
        (10.0, 88.0, 0.0),
        (30.0, -86.0, 5.0),
    ]
}

/// The standard suite: realistic lens geometries (a fisheye seeing 110° across the short side of
/// a portrait frame, in three projection families the solver is not told), a five-row rectilinear
/// sphere of low-texture photos (where SIFT alone leaves most photos unconnected) and a partial
/// rectilinear panorama.
pub fn standard_suite(megapixels: f64) -> Vec<Case> {
    let scale = |mut l: Lens, mp: f64| {
        let k = (mp * 1e6 / (l.width as f64 * l.height as f64)).sqrt();
        l.width = (l.width as f64 * k).round() as u32;
        l.height = (l.height as f64 * k).round() as u32;
        l
    };
    let mut rows = Vec::new();
    for (pitch, n) in [(0.0, 9), (38.0, 7), (-38.0, 7), (72.0, 4), (-72.0, 4)] {
        for k in 0..n {
            rows.push((k as f64 * 360.0 / n as f64 + pitch * 0.3, pitch + (k % 3) as f64 - 1.0, (k as f64 * 1.7) % 4.0 - 2.0));
        }
    }
    let mut rect = Lens::new(LensKind::Rectilinear, 65.0, 1200, 800);
    rect.a = 0.015;
    rect.b = -0.05;
    rect.d = 9.0;
    rect.e = -6.0;
    let mut partial = Lens::new(LensKind::Rectilinear, 60.0, 1500, 1000);
    partial.b = -0.04;
    vec![
        Case {
            name: "fisheye-equidistant",
            description: "8 fisheye frames, equidistant projection",
            lens: scale(fisheye(LensKind::Fisheye), megapixels),
            poses: fisheye_poses(),
            exif_focal: false,
        },
        Case {
            name: "fisheye-equisolid",
            description: "8 fisheye frames, equisolid projection (most real fisheyes)",
            lens: scale(fisheye(LensKind::FisheyeEquisolid), megapixels),
            poses: fisheye_poses(),
            exif_focal: false,
        },
        Case {
            name: "fisheye-stereographic",
            description: "8 fisheye frames, stereographic projection",
            lens: scale(fisheye(LensKind::FisheyeStereographic), megapixels),
            poses: fisheye_poses(),
            exif_focal: false,
        },
        Case {
            name: "low-texture-sphere",
            description: "31 rectilinear frames in five rows, little texture",
            lens: rect,
            poses: rows,
            exif_focal: false,
        },
        Case {
            name: "partial-rectilinear",
            description: "7 rectilinear frames, two rows, focal length from EXIF",
            lens: scale(partial, megapixels.min(6.0)),
            poses: vec![
                (-50.0, 2.0, 0.5),
                (-25.0, -1.0, -1.0),
                (0.0, 1.0, 1.0),
                (25.0, 0.0, 0.0),
                (50.0, -2.0, -0.5),
                (-12.0, 28.0, 1.0),
                (14.0, 27.0, -1.0),
            ],
            exif_focal: true,
        },
    ]
}

/// Renders the photos of a case.
pub fn render_case(case: &Case) -> (Vec<Rotation3<f64>>, Vec<Arc<image::RgbImage>>) {
    let truth: Vec<Rotation3<f64>> = case.poses.iter().map(|&(y, p, r)| rotation_from_ypr(y, p, r)).collect();
    // About one panorama pixel per photo pixel at the frame centre.
    let px_per_deg = case.lens.model().f * 1f64.to_radians();
    let pano_w = ((px_per_deg * 360.0) as u32).clamp(4096, 16384).next_multiple_of(2);
    let pano = procedural_pano(pano_w, 7);
    let shots = truth
        .par_iter()
        .enumerate()
        .map(|(i, r)| {
            let opts = ShotOptions { gain: 1.0 + 0.15 * ((i % 3) as f32 - 1.0), vignette: 0.25, noise: 2.0, seed: i as u64 };
            Arc::new(render_shot(&pano, &case.lens, r, &opts))
        })
        .collect();
    (truth, shots)
}

/// Solves `q + D(q) = p` for the unwarped position (the renderer samples `t + D(t)`).
fn unwarp(field: Option<&WarpField>, x: f64, y: f64, w: f64, h: f64) -> (f64, f64) {
    let Some(f) = field else { return (x, y) };
    let (mut sx, mut sy) = (x, y);
    for _ in 0..6 {
        let (dx, dy) = f.sample(sx as f32, sy as f32, w as f32, h as f32);
        sx = x - dx as f64;
        sy = y - dy as f64;
    }
    (sx, sy)
}

fn summary(mut v: Vec<f64>) -> Spread {
    if v.is_empty() {
        return (0.0, 0.0, 0.0, 0.0);
    }
    v.sort_by(|a, b| a.total_cmp(b));
    let rms = (v.iter().map(|x| x * x).sum::<f64>() / v.len() as f64).sqrt();
    let at = |p: f64| v[((v.len() - 1) as f64 * p).round() as usize];
    (rms, at(0.5), at(0.95), v[v.len() - 1])
}

/// True misregistration of a solved project against ground truth: (all overlaps, frame edges),
/// each (rms, median, p95, max) in source pixels at the frame centre.
pub fn misregistration(truth_lens: &Lens, truth: &[Rotation3<f64>], project: &Project) -> (Spread, Spread) {
    let mt = truth_lens.model();
    let est: Vec<_> = project.images.iter().map(|i| project.lenses[i.lens].model()).collect();
    let rots: Vec<_> = project.images.iter().map(|i| i.rot()).collect();
    let px_angle = 1.0 / mt.f;
    let n = 60_000usize;
    let golden = std::f64::consts::PI * (3.0 - 5f64.sqrt());
    let (w, h) = (truth_lens.width as f64, truth_lens.height as f64);
    let samples: Vec<(f64, bool)> = (0..n)
        .into_par_iter()
        .flat_map_iter(|k| {
            let y = 1.0 - 2.0 * (k as f64 + 0.5) / n as f64;
            let r = (1.0 - y * y).sqrt();
            let t = golden * k as f64;
            let d = V3::new(t.cos() * r, y, t.sin() * r);
            let mut placed: Vec<(V3, bool)> = Vec::new();
            for i in 0..truth.len() {
                if !project.images[i].aligned || project.images[i].estimated {
                    continue;
                }
                let Some((px, py)) = mt.ray_to_pixel(truth[i].inverse_transform_vector(&d)) else { continue };
                if px < 2.0 || py < 2.0 || px > w - 2.0 || py > h - 2.0 {
                    continue;
                }
                let rr = ((px - w / 2.0).powi(2) + (py - h / 2.0).powi(2)).sqrt() / (0.5 * (w * w + h * h).sqrt());
                let (qx, qy) = unwarp(project.images[i].warp.as_ref(), px, py, w, h);
                placed.push((rots[i] * est[i].pixel_to_ray(qx, qy), rr > 0.7));
            }
            let mut out = Vec::new();
            for a in 0..placed.len() {
                for b in a + 1..placed.len() {
                    let ang = placed[a].0.cross(&placed[b].0).norm().atan2(placed[a].0.dot(&placed[b].0));
                    out.push((ang / px_angle, placed[a].1 || placed[b].1));
                }
            }
            out
        })
        .collect();
    let all = summary(samples.iter().map(|s| s.0).collect());
    let edges = summary(samples.iter().filter(|s| s.1).map(|s| s.0).collect());
    (all, edges)
}

/// Runs one case end to end. `rescue` builds the learned rescue matcher for the case's photos.
pub fn run_case(case: &Case, rescue: Option<RescueFactory>, progress: Progress) -> anyhow::Result<CaseResult> {
    progress(0.0, &format!("{}: rendering photos", case.name));
    let (truth, shots) = render_case(case);
    let exif = crate::project::ExifInfo { focal_35mm: case.exif_focal.then(|| case.lens.focal_35mm()), ..Default::default() };
    let sources: Vec<Source> = shots
        .par_iter()
        .enumerate()
        .map(|(i, s)| Source::from_rgb8(format!("{}-{i:02}.png", case.name), s.clone(), exif.clone(), &DetectOptions::default()))
        .collect();
    drop(shots);
    let mut opts = AlignOptions::default();
    if let Some(make) = rescue {
        opts.rescue = Some(make(&sources));
    }
    progress(0.2, &format!("{}: aligning", case.name));
    let t = Instant::now();
    let (project, report) = crate::align_sources(&sources, &opts, &|f, s| progress(0.2 + 0.7 * f, &format!("{}: {s}", case.name)))?;
    let align_seconds = t.elapsed().as_secs_f32();
    let (all, edges) = misregistration(&case.lens, &truth, &project);
    let lens = project.lenses[0];
    Ok(CaseResult {
        name: case.name.to_string(),
        description: case.description.to_string(),
        photos: project.images.len(),
        placed: project.images.iter().filter(|i| i.aligned && !i.estimated).count(),
        estimated: report.estimated.len(),
        learned_pairs: report.pairs.iter().filter(|p| p.learned).count(),
        align_seconds,
        all,
        edges,
        lens: format!("{} {:.1}°", lens.projection_label(), lens.hfov),
        true_lens: format!("{} {:.1}°", case.lens.projection_label(), case.lens.hfov),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metric_is_zero_for_the_true_model() {
        let case = &standard_suite(0.3)[1];
        let truth: Vec<Rotation3<f64>> = case.poses.iter().map(|&(y, p, r)| rotation_from_ypr(y, p, r)).collect();
        let meta: Vec<_> =
            (0..truth.len()).map(|i| (format!("{i}").into(), case.lens.width, case.lens.height, Default::default())).collect();
        let mut project = crate::align::new_project(&meta);
        project.lenses[0] = case.lens;
        for (img, r) in project.images.iter_mut().zip(&truth) {
            img.set_rot(r);
            img.aligned = true;
        }
        let (all, edges) = misregistration(&case.lens, &truth, &project);
        assert!(all.3 < 1e-6 && edges.3 < 1e-6, "{all:?} {edges:?}");
        // A one-degree error in one photo shows up.
        project.images[0].set_rot(&(truth[0] * Rotation3::from_axis_angle(&V3::y_axis(), 1f64.to_radians())));
        let (all, _) = misregistration(&case.lens, &truth, &project);
        assert!(all.3 > 3.0, "{all:?}");
    }
}

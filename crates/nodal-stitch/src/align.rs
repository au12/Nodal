//! Automatic alignment: pair discovery, lens estimation, initial orientations, staged bundle
//! adjustment with outlier rejection, guided re-matching and levelling.

use crate::features::FeatureSet;
use crate::lens::{Lens, LensKind, LensModel, V3};
use crate::matching::{match_descriptors, match_guided, ransac_rotation, rays_for, RawMatch};
use crate::optimize::{self, LensFlags, OptOptions};
use crate::project::{ControlPoint, ExifInfo, Project, SourceImage};
use crate::Progress;
use nalgebra::{Matrix3, Rotation3, SymmetricEigen};
use rayon::prelude::*;
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct AlignOptions {
    /// Force a lens instead of using EXIF or estimating: (kind, horizontal field of view in degrees).
    pub lens: Option<(LensKind, f64)>,
    /// Number of strongest features used to discover which images overlap.
    pub quick_features: usize,
    pub ratio: f32,
    /// Calibrate field of view and distortion from the images.
    pub optimize_lens: bool,
    pub level: bool,
    /// Control points kept per image pair.
    pub max_points_per_pair: usize,
}

impl Default for AlignOptions {
    fn default() -> Self {
        AlignOptions { lens: None, quick_features: 900, ratio: 0.8, optimize_lens: true, level: true, max_points_per_pair: 400 }
    }
}

#[derive(Clone, Debug)]
pub struct PairStat {
    pub a: usize,
    pub b: usize,
    pub matches: usize,
    pub inliers: usize,
}

#[derive(Clone, Debug, Default)]
pub struct AlignReport {
    pub pairs: Vec<PairStat>,
    pub control_points: usize,
    /// RMS / maximum control point distance in source pixels.
    pub rms: f64,
    pub max: f64,
    pub unaligned: Vec<usize>,
    /// Where each lens's starting field of view came from.
    pub lens_source: Vec<LensSource>,
    pub log: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LensSource {
    Exif,
    Estimated,
    User,
}

fn guess_kind(exif: &ExifInfo) -> Option<LensKind> {
    let name = format!("{} {}", exif.lens.as_deref().unwrap_or(""), exif.model.as_deref().unwrap_or("")).to_lowercase();
    if name.contains("fisheye") || name.contains("fish-eye") || name.contains("fish eye") {
        return Some(LensKind::Fisheye);
    }
    match exif.focal_35mm {
        Some(f) if f < 8.5 => Some(LensKind::Fisheye),
        Some(f) if f >= 16.5 => Some(LensKind::Rectilinear),
        _ => None,
    }
}

/// Builds a project from image metadata, grouping images that share a lens.
pub fn new_project(images: &[(PathBuf, u32, u32, ExifInfo)]) -> Project {
    let mut project = Project::default();
    let mut keys: Vec<(u32, u32, i64, i64, String)> = Vec::new();
    for (path, w, h, exif) in images {
        let key = (
            *w,
            *h,
            (exif.focal_mm.unwrap_or(0.0) * 10.0).round() as i64,
            (exif.focal_35mm.unwrap_or(0.0) * 10.0).round() as i64,
            exif.lens.clone().unwrap_or_default(),
        );
        let lens = match keys.iter().position(|k| *k == key) {
            Some(i) => i,
            None => {
                keys.push(key);
                let kind = guess_kind(exif).unwrap_or(LensKind::Rectilinear);
                let hfov = match exif.focal_35mm {
                    Some(f35) => Lens::hfov_from_35mm(kind, f35, *w, *h),
                    None => 50.0,
                };
                project.lenses.push(Lens::new(kind, hfov.clamp(2.0, 300.0), *w, *h));
                project.lenses.len() - 1
            }
        };
        project.images.push(SourceImage {
            path: path.clone(),
            name: path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
            width: *w,
            height: *h,
            exif: exif.clone(),
            lens,
            rotation: [0.0, 0.0, 0.0, 1.0],
            aligned: false,
            enabled: true,
            gain: [1.0; 3],
            warp: None,
        });
    }
    project
}

struct Pair {
    a: usize,
    b: usize,
    raw: Vec<RawMatch>,
}

fn feature_px(features: &[FeatureSet], img: usize) -> f64 {
    // Localisation noise scales with the detection scale; estimate it from the smallest features.
    let mut s: Vec<f32> = features[img].feats.iter().map(|f| f.scale).collect();
    if s.is_empty() {
        return 1.0;
    }
    let k = s.len() / 10;
    s.select_nth_unstable_by(k, |a, b| a.total_cmp(b));
    (s[k] as f64 / 1.6).max(1.0)
}

fn pair_inliers(features: &[FeatureSet], p: &Pair, ma: &LensModel, mb: &LensModel, tol_px: f64) -> Option<crate::matching::RotationFit> {
    let rays = rays_for(&features[p.a], &features[p.b], &p.raw, ma, mb);
    let tol = tol_px / ma.f.min(mb.f);
    ransac_rotation(&rays, tol, (p.a * 7919 + p.b) as u64)
}

/// Scores a lens hypothesis by how many matches agree with a pure rotation.
fn score_lens(features: &[FeatureSet], pairs: &[&Pair], project: &Project, lens_idx: usize, cand: &Lens, tol_px: f64) -> usize {
    let models: Vec<LensModel> = project.lenses.iter().enumerate().map(|(i, l)| if i == lens_idx { cand.model() } else { l.model() }).collect();
    pairs
        .par_iter()
        .map(|p| {
            let (ma, mb) = (&models[project.images[p.a].lens], &models[project.images[p.b].lens]);
            pair_inliers(features, p, ma, mb, tol_px).map(|f| f.inliers.len()).unwrap_or(0)
        })
        .sum()
}

fn estimate_lens(features: &[FeatureSet], pairs: &[Pair], project: &Project, lens_idx: usize, kinds: &[LensKind], tol_px: f64) -> Option<(Lens, usize)> {
    let involved: Vec<&Pair> = pairs.iter().filter(|p| project.images[p.a].lens == lens_idx || project.images[p.b].lens == lens_idx).collect();
    if involved.is_empty() {
        return None;
    }
    let base = project.lenses[lens_idx];
    let mut best: Option<(Lens, usize)> = None;
    let mut best_rect = 0usize;
    for &kind in kinds {
        let grid: &[f64] = if kind.is_fisheye() { &[90.0, 105.0, 120.0, 140.0, 160.0, 180.0, 200.0] } else { &[10.0, 14.0, 19.0, 25.0, 32.0, 40.0, 49.0, 58.0, 67.0, 77.0, 88.0, 100.0, 112.0] };
        let mut scored: Vec<(f64, usize)> = grid
            .iter()
            .map(|&v| {
                let mut l = base;
                l.kind = kind;
                l.hfov = v;
                (v, score_lens(features, &involved, project, lens_idx, &l, tol_px))
            })
            .collect();
        // Among near-ties prefer an ordinary field of view: flat scores mean it is unobservable.
        let top = scored.iter().map(|s| s.1).max().unwrap_or(0);
        scored.retain(|s| s.1 as f64 >= top as f64 * 0.97);
        let pref = if kind.is_fisheye() { 180.0 } else { 60.0 };
        scored.sort_by(|a, b| (a.0 - pref).abs().total_cmp(&(b.0 - pref).abs()));
        let (mut v, mut sc) = scored[0];
        // Local refinement around the best grid point.
        let mut step = v * 0.12;
        for _ in 0..5 {
            for cand in [v - step, v + step] {
                if cand < 4.0 || cand > 300.0 || (!kind.is_fisheye() && cand > 150.0) {
                    continue;
                }
                let mut l = base;
                l.kind = kind;
                l.hfov = cand;
                let s = score_lens(features, &involved, project, lens_idx, &l, tol_px);
                if s > sc + sc / 100 {
                    (v, sc) = (cand, s);
                }
            }
            step *= 0.5;
        }
        if !kind.is_fisheye() {
            best_rect = sc;
        }
        // A fisheye must explain the matches clearly better than a rectilinear lens to be chosen.
        let wins = match &best {
            None => true,
            Some((_, bs)) => {
                if kind.is_fisheye() {
                    sc as f64 > best_rect.max(*bs) as f64 * 1.2
                } else {
                    sc > *bs
                }
            }
        };
        if wins {
            let mut l = base;
            l.kind = kind;
            l.hfov = v;
            best = Some((l, sc));
        }
    }
    best
}

fn thin_points(points: Vec<ControlPoint>, max: usize, w: f32, h: f32) -> Vec<ControlPoint> {
    if points.len() <= max {
        return points;
    }
    // Round-robin over a grid on image a so the kept points stay spread out.
    const G: usize = 12;
    let mut cells: Vec<Vec<ControlPoint>> = vec![Vec::new(); G * G];
    for p in points {
        let gx = ((p.ax / w * G as f32) as usize).min(G - 1);
        let gy = ((p.ay / h * G as f32) as usize).min(G - 1);
        cells[gy * G + gx].push(p);
    }
    let mut out = Vec::with_capacity(max);
    let mut depth = 0;
    while out.len() < max {
        let mut any = false;
        for c in &cells {
            if let Some(p) = c.get(depth) {
                any = true;
                out.push(*p);
                if out.len() == max {
                    break;
                }
            }
        }
        if !any {
            break;
        }
        depth += 1;
    }
    out
}

fn median(mut v: Vec<f32>) -> f32 {
    if v.is_empty() {
        return 0.0;
    }
    let k = v.len() / 2;
    v.select_nth_unstable_by(k, |a, b| a.total_cmp(b));
    v[k]
}

/// Drops control points whose distance is far above the typical one. Returns how many were removed.
fn prune(project: &mut Project, floor_px: f32, sigmas: f32) -> usize {
    optimize::update_errors(project);
    let med = median(project.control_points.iter().map(|c| c.error).collect());
    let thr = (med * sigmas).max(floor_px);
    let before = project.control_points.len();
    project.control_points.retain(|c| c.error <= thr);
    before - project.control_points.len()
}

/// Rotates the world so the horizon is level and the panorama is centred.
pub fn level(project: &mut Project) {
    let active: Vec<usize> = (0..project.images.len()).filter(|&i| project.images[i].aligned).collect();
    if active.is_empty() {
        return;
    }
    let rots: Vec<Rotation3<f64>> = active.iter().map(|&i| project.images[i].rot()).collect();
    let mean_up: V3 = rots.iter().map(|r| r * V3::y()).sum::<V3>();
    let mut up = if mean_up.norm() > 1e-6 { mean_up.normalize() } else { V3::y() };
    if active.len() >= 2 {
        // Photographers keep the camera's horizontal axis level while turning, so the right
        // vectors of all shots span the horizon plane; its normal is "up" (Brown & Lowe).
        let mut c = Matrix3::<f64>::zeros();
        for r in &rots {
            let x: V3 = r * V3::x();
            c += x * x.transpose();
        }
        let eig = SymmetricEigen::new(c);
        let mut order = [0usize, 1, 2];
        order.sort_by(|&a, &b| eig.eigenvalues[a].total_cmp(&eig.eigenvalues[b]));
        let (l0, l1) = (eig.eigenvalues[order[0]], eig.eigenvalues[order[1]]);
        // Only trust the plane fit when it is well defined (the shots actually turn).
        if l1 > 1e-3 * active.len() as f64 && l0 < 0.35 * l1 {
            let n: V3 = eig.eigenvectors.column(order[0]).into_owned();
            let n = if n.dot(&up) < 0.0 { -n } else { n };
            // Blend towards the plane normal unless it disagrees wildly with the cameras' own up.
            if n.dot(&up) > 0.5 {
                up = n;
            }
        }
    }
    // Centre: partial panoramas on their mean direction, full spheres on the first image.
    let mean_fwd: V3 = rots.iter().map(|r| r * V3::z()).sum::<V3>() / rots.len() as f64;
    let mut fwd = if mean_fwd.norm() > 0.3 { mean_fwd } else { rots[0] * V3::z() };
    fwd -= up * fwd.dot(&up);
    if fwd.norm() < 1e-6 {
        fwd = rots[0] * V3::x();
        fwd -= up * fwd.dot(&up);
    }
    let z = fwd.normalize();
    let x = up.cross(&z).normalize();
    let g = Rotation3::from_matrix_unchecked(Matrix3::from_rows(&[x.transpose(), up.transpose(), z.transpose()]));
    project.rotate_world(&g);
}

/// Aligns the project's images from their features. `project` should come from [`new_project`].
pub fn align(project: &mut Project, features: &[FeatureSet], opts: &AlignOptions, progress: Progress) -> anyhow::Result<AlignReport> {
    let n = project.images.len();
    anyhow::ensure!(n == features.len(), "one feature set per image");
    let mut report = AlignReport::default();
    for img in &mut project.images {
        img.aligned = false;
        img.warp = None;
        img.rotation = [0.0, 0.0, 0.0, 1.0];
    }
    project.control_points.clear();
    if n == 0 {
        return Ok(report);
    }
    if n == 1 {
        project.images[0].aligned = true;
        report.lens_source = vec![LensSource::Exif; project.lenses.len()];
        return Ok(report);
    }
    let px: Vec<f64> = (0..n).map(|i| feature_px(features, i)).collect();
    let px_mean = px.iter().sum::<f64>() / n as f64;

    // 1. Which images overlap? Match the strongest features of every pair.
    progress(0.0, "Finding overlapping images");
    let heads: Vec<FeatureSet> = features.iter().map(|f| f.head(opts.quick_features)).collect();
    let all_pairs: Vec<(usize, usize)> = (0..n).flat_map(|a| (a + 1..n).map(move |b| (a, b))).collect();
    let quick: Vec<Pair> = all_pairs
        .par_iter()
        .map(|&(a, b)| Pair { a, b, raw: match_descriptors(&heads[a], &heads[b], opts.ratio) })
        .filter(|p| p.raw.len() >= 8)
        .collect();

    // 2. Lens: user override, EXIF, or estimated from the matches.
    progress(0.15, "Estimating lens");
    report.lens_source = vec![LensSource::Exif; project.lenses.len()];
    for li in 0..project.lenses.len() {
        let exif = project.images.iter().find(|i| i.lens == li).map(|i| i.exif.clone()).unwrap_or_default();
        if let Some((kind, hfov)) = opts.lens {
            project.lenses[li].kind = kind;
            project.lenses[li].hfov = hfov;
            report.lens_source[li] = LensSource::User;
            continue;
        }
        let kinds: Vec<LensKind> = match (exif.focal_35mm, guess_kind(&exif)) {
            (Some(_), Some(_)) => continue, // EXIF is enough
            (Some(_), None) => vec![LensKind::Rectilinear, LensKind::Fisheye],
            (None, Some(k)) => vec![k],
            (None, None) => vec![LensKind::Rectilinear, LensKind::Fisheye],
        };
        if exif.focal_35mm.is_some() {
            // Focal length known but the mapping is ambiguous (very wide lens): test both mappings at that focal length.
            let f35 = exif.focal_35mm.unwrap();
            let l0 = project.lenses[li];
            let involved: Vec<&Pair> = quick.iter().filter(|p| project.images[p.a].lens == li || project.images[p.b].lens == li).collect();
            let mut best = (l0, 0usize);
            for kind in kinds {
                let mut l = l0;
                l.kind = kind;
                l.hfov = Lens::hfov_from_35mm(kind, f35, l0.width, l0.height).clamp(2.0, 300.0);
                let s = score_lens(features, &involved, project, li, &l, 2.5 * px_mean);
                if s > best.1 {
                    best = (l, s);
                }
            }
            project.lenses[li] = best.0;
            continue;
        }
        report.lens_source[li] = LensSource::Estimated;
        if let Some((l, score)) = estimate_lens(features, &quick, project, li, &kinds, 2.5 * px_mean) {
            report.log.push(format!("Lens {}: estimated {} {:.1}° from {} agreeing matches", li + 1, l.kind.label(), l.hfov, score));
            project.lenses[li] = l;
        }
    }

    // 3. Full matching and verification of promising pairs.
    progress(0.3, "Matching control points");
    let models: Vec<LensModel> = project.lenses.iter().map(|l| l.model()).collect();
    let lens_of = |i: usize| &models[project.images[i].lens];
    let loose = 4.0 * px_mean;
    let promising: Vec<(usize, usize)> = quick
        .par_iter()
        .filter(|p| pair_inliers(features, p, lens_of(p.a), lens_of(p.b), loose).is_some_and(|f| f.inliers.len() >= 6))
        .map(|p| (p.a, p.b))
        .collect();
    let verify = |a: usize, b: usize| -> Option<(Pair, crate::matching::RotationFit)> {
        let pair = Pair { a, b, raw: match_descriptors(&features[a], &features[b], opts.ratio) };
        let fit = pair_inliers(features, &pair, lens_of(a), lens_of(b), loose)?;
        (fit.inliers.len() as f64 > 6.0 + 0.22 * pair.raw.len() as f64).then_some((pair, fit))
    };
    let mut verified: Vec<(Pair, crate::matching::RotationFit)> = promising.par_iter().filter_map(|&(a, b)| verify(a, b)).collect();

    // Images left unconnected get a full match against everything (the quick pass can miss thin overlaps).
    let connected = |verified: &[(Pair, crate::matching::RotationFit)]| {
        let mut comp: Vec<usize> = (0..n).collect();
        fn find(c: &mut [usize], i: usize) -> usize {
            let mut r = i;
            while c[r] != r {
                r = c[r];
            }
            c[i] = r;
            r
        }
        for (p, _) in verified {
            let (ra, rb) = (find(&mut comp, p.a), find(&mut comp, p.b));
            comp[ra] = rb;
        }
        (0..n).map(|i| find(&mut comp, i)).collect::<Vec<_>>()
    };
    let comp = connected(&verified);
    if comp.iter().any(|&c| c != comp[0]) {
        progress(0.45, "Searching for more overlaps");
        let tried: std::collections::HashSet<(usize, usize)> = promising.iter().copied().collect();
        let extra: Vec<(usize, usize)> = all_pairs.iter().copied().filter(|&(a, b)| comp[a] != comp[b] && !tried.contains(&(a, b))).collect();
        verified.extend(extra.par_iter().filter_map(|&(a, b)| verify(a, b)).collect::<Vec<_>>());
    }
    verified.sort_by_key(|(p, _)| (p.a, p.b));
    for (p, fit) in &verified {
        report.pairs.push(PairStat { a: p.a, b: p.b, matches: p.raw.len(), inliers: fit.inliers.len() });
    }
    anyhow::ensure!(!verified.is_empty(), "No overlapping images were found. Check that the photos overlap by at least 20–30 % and that the lens type is right.");

    // 4. Initial orientations: grow a maximum spanning tree from the best-connected image.
    progress(0.5, "Placing images");
    let comp = connected(&verified);
    let mut comp_weight = std::collections::HashMap::<usize, usize>::new();
    let mut degree = vec![0usize; n];
    for (p, fit) in &verified {
        *comp_weight.entry(comp[p.a]).or_default() += fit.inliers.len();
        degree[p.a] += fit.inliers.len();
        degree[p.b] += fit.inliers.len();
    }
    let main = *comp_weight.iter().max_by_key(|(c, w)| (**w, usize::MAX - **c)).unwrap().0;
    let anchor = (0..n).filter(|&i| comp[i] == main).max_by_key(|&i| (degree[i], usize::MAX - i)).unwrap();
    let mut rots: Vec<Option<Rotation3<f64>>> = vec![None; n];
    rots[anchor] = Some(Rotation3::identity());
    loop {
        // Strongest edge leaving the placed set.
        let next = verified
            .iter()
            .filter(|(p, _)| rots[p.a].is_some() != rots[p.b].is_some())
            .max_by_key(|(_, f)| f.inliers.len());
        let Some((p, fit)) = next else { break };
        if let Some(ra) = rots[p.a] {
            rots[p.b] = Some(ra * fit.r);
        } else {
            rots[p.a] = Some(rots[p.b].unwrap() * fit.r.inverse());
        }
    }
    for i in 0..n {
        if let Some(r) = rots[i] {
            project.images[i].set_rot(&r);
            project.images[i].aligned = true;
        } else {
            report.unaligned.push(i);
        }
    }

    // 5. Control points from the verified matches.
    for (p, fit) in &verified {
        if !(project.images[p.a].aligned && project.images[p.b].aligned) {
            continue;
        }
        let pts: Vec<ControlPoint> = fit
            .inliers
            .iter()
            .map(|&k| {
                let m = p.raw[k];
                let (fa, fb) = (&features[p.a].feats[m.a as usize], &features[p.b].feats[m.b as usize]);
                ControlPoint { a: p.a as u32, b: p.b as u32, ax: fa.x, ay: fa.y, bx: fb.x, by: fb.y, error: 0.0 }
            })
            .collect();
        let (w, h) = (project.images[p.a].width as f32, project.images[p.a].height as f32);
        project.control_points.extend(thin_points(pts, opts.max_points_per_pair, w, h));
    }

    // 6. Bundle adjustment in stages, pruning outliers as the model gets more precise.
    progress(0.6, "Optimising");
    let mut fixed = vec![false; n];
    fixed[anchor] = true;
    let fov_prior: Vec<Option<(f64, f64)>> = project
        .lenses
        .iter()
        .zip(&report.lens_source)
        .map(|(l, src)| match src {
            LensSource::Exif => Some((l.hfov, (l.hfov * 0.04).max(0.5))),
            LensSource::User => Some((l.hfov, (l.hfov * 0.01).max(0.1))),
            LensSource::Estimated => Some((l.hfov, l.hfov * 0.5)),
        })
        .collect();
    let run = |project: &mut Project, flags: LensFlags, huber: f64| {
        let cps = project.control_points.clone();
        optimize::optimize(project, &cps, &OptOptions { flags, huber_px: huber, max_iters: 60, fov_prior: fov_prior.clone(), fixed: fixed.clone() })
    };
    let floor = (1.5 * px_mean) as f32;
    let r = run(project, LensFlags::NONE, 3.0 * px_mean);
    report.log.push(format!("Orientations: {} points, RMS {:.2} px", project.control_points.len(), r.rms));
    prune(project, floor * 4.0, 4.0);
    let user_lens = opts.lens.is_some();
    if opts.optimize_lens {
        let per_lens = project.control_points.len() / project.lenses.len().max(1);
        let stages: [(LensFlags, usize, &str); 4] = [
            (LensFlags { fov: !user_lens, ..LensFlags::NONE }, 12, "field of view"),
            (LensFlags { fov: !user_lens, b: true, ..LensFlags::NONE }, 30, "distortion b"),
            (LensFlags { fov: !user_lens, a: true, b: true, c: true, shift: false }, 80, "distortion a, b, c"),
            (LensFlags { fov: !user_lens, a: true, b: true, c: true, shift: true }, 150, "lens shift"),
        ];
        for (i, (flags, need, label)) in stages.iter().enumerate() {
            if per_lens < *need || (*flags == LensFlags::NONE) {
                continue;
            }
            progress(0.6 + 0.07 * (i + 1) as f32, "Calibrating lens");
            let backup = project.clone();
            let before = optimize::error_stats(project, &project.control_points, &project.images.iter().map(|i| i.lens).collect::<Vec<_>>()).0;
            let r = run(project, *flags, 2.5 * px_mean);
            let sane = project.lenses.iter().all(|l| l.hfov.is_finite() && l.hfov > 1.0 && (l.a.abs() + l.b.abs() + l.c.abs()) < 0.9);
            if !sane || r.rms > before * 1.001 {
                *project = backup;
                continue;
            }
            report.log.push(format!("Calibrated {label}: RMS {:.2} → {:.2} px", before, r.rms));
            prune(project, floor * 2.5, 3.5);
        }
    } else {
        run(project, LensFlags::NONE, 2.5 * px_mean);
        prune(project, floor * 2.5, 3.5);
    }

    // 7. Guided re-matching: with the geometry known, look for many more correspondences near
    //    their predicted positions, in every pair of images that overlap.
    progress(0.9, "Refining control points");
    let added = guided_points(project, features, opts, 5.0 * px_mean);
    if added > 0 {
        let flags = if opts.optimize_lens { LensFlags { fov: !user_lens, a: true, b: true, c: true, shift: project.control_points.len() / project.lenses.len().max(1) >= 150 } } else { LensFlags::NONE };
        let backup = project.clone();
        let r = run(project, flags, 2.0 * px_mean);
        let sane = project.lenses.iter().all(|l| l.hfov.is_finite() && (l.a.abs() + l.b.abs() + l.c.abs()) < 0.9);
        if !sane {
            *project = backup;
        } else {
            report.log.push(format!("Guided matching: {} points, RMS {:.2} px", project.control_points.len(), r.rms));
        }
        prune(project, floor * 2.0, 3.0);
        run(project, flags, 2.0 * px_mean);
    }

    if opts.level {
        level(project);
    }
    optimize::update_errors(project);
    let lens_idx: Vec<usize> = project.images.iter().map(|i| i.lens).collect();
    let (rms, max) = optimize::error_stats(project, &project.control_points, &lens_idx);
    report.rms = rms;
    report.max = max;
    report.control_points = project.control_points.len();
    progress(1.0, "Aligned");
    Ok(report)
}

/// Replaces the control points with a denser set found by geometry-guided matching.
/// Returns the number of points found.
fn guided_points(project: &mut Project, features: &[FeatureSet], opts: &AlignOptions, radius_px: f64) -> usize {
    let n = project.images.len();
    let models: Vec<LensModel> = project.lenses.iter().map(|l| l.model()).collect();
    let rots: Vec<Rotation3<f64>> = project.images.iter().map(|i| i.rot()).collect();
    let pairs: Vec<(usize, usize)> = (0..n)
        .flat_map(|a| (a + 1..n).map(move |b| (a, b)))
        .filter(|&(a, b)| project.images[a].aligned && project.images[b].aligned)
        .collect();
    let found: Vec<Vec<ControlPoint>> = pairs
        .par_iter()
        .map(|&(a, b)| {
            let (ma, mb) = (&models[project.images[a].lens], &models[project.images[b].lens]);
            let rel = rots[b].inverse() * rots[a];
            let predict = |x: f32, y: f32| {
                let ray = rel * ma.pixel_to_ray(x as f64, y as f64);
                let (px, py) = mb.ray_to_pixel(ray)?;
                mb.contains(px, py).then_some((px as f32, py as f32))
            };
            let raw = match_guided(&features[a], &features[b], predict, radius_px as f32, 0.92, 90_000);
            if raw.len() < 6 {
                return Vec::new();
            }
            let pts: Vec<ControlPoint> = raw
                .iter()
                .map(|m| {
                    let (fa, fb) = (&features[a].feats[m.a as usize], &features[b].feats[m.b as usize]);
                    ControlPoint { a: a as u32, b: b as u32, ax: fa.x, ay: fa.y, bx: fb.x, by: fb.y, error: 0.0 }
                })
                .collect();
            thin_points(pts, opts.max_points_per_pair, project.images[a].width as f32, project.images[a].height as f32)
        })
        .collect();
    let total: usize = found.iter().map(|v| v.len()).sum();
    if total > project.control_points.len() {
        project.control_points = found.into_iter().flatten().collect();
        total
    } else {
        0
    }
}

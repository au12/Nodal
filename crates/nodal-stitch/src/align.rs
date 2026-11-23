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
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

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
    /// Measure every overlap densely after the global alignment (second adjustment, local warps,
    /// parallax diagnostics).
    pub dense: bool,
    /// Second matcher for photos the SIFT pass leaves unconnected.
    pub rescue: Option<RescueHandle>,
}

impl Default for AlignOptions {
    fn default() -> Self {
        AlignOptions {
            lens: None,
            quick_features: 900,
            ratio: 0.8,
            optimize_lens: true,
            level: true,
            max_points_per_pair: 400,
            dense: true,
            rescue: None,
        }
    }
}

/// A second matcher for photo pairs SIFT could not connect (low texture, repeated patterns, big
/// exposure or viewpoint changes). `ml::SourceRescue` implements it with learned features.
pub trait Rescue: Send + Sync {
    fn name(&self) -> &str;
    /// How likely each pair is to overlap (higher is likelier); used to choose which pairs to match.
    fn affinity(&self, pairs: &[(usize, usize)], progress: &dyn Fn(f32, &str)) -> anyhow::Result<Vec<f32>>;
    /// Correspondences for each pair, in full-resolution pixels. `thorough` asks for the slower,
    /// stronger matcher (used for the pairs the fast one could not verify).
    fn correspondences(&self, pairs: &[(usize, usize)], thorough: bool, progress: &dyn Fn(f32, &str))
        -> anyhow::Result<Vec<RescueMatches>>;
}

#[derive(Clone, Debug, Default)]
pub struct RescueMatches {
    /// (ax, ay, bx, by) in full-resolution pixels.
    pub pts: Vec<[f32; 4]>,
    /// Typical localisation error in full-resolution pixels.
    pub sigma: f32,
}

#[derive(Clone)]
pub struct RescueHandle(pub Arc<dyn Rescue>);

impl std::fmt::Debug for RescueHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Rescue({})", self.0.name())
    }
}

#[derive(Clone, Debug)]
pub struct PairStat {
    pub a: usize,
    pub b: usize,
    pub matches: usize,
    pub inliers: usize,
    /// Found by the learned rescue matcher rather than SIFT.
    pub learned: bool,
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
    /// Result of the local alignment pass, when it ran.
    pub local: Option<crate::local::LocalReport>,
    /// Dense overlap registration, when it ran.
    pub dense: Option<crate::dense::DenseReport>,
    /// Exposure, white balance and vignetting calibration.
    pub photometric: Option<crate::photometric::PhotoReport>,
    /// (stage, seconds)
    pub timings: Vec<(String, f32)>,
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
            mask: None,
        });
    }
    project
}

struct Pair {
    a: usize,
    b: usize,
    raw: Vec<RawMatch>,
}

/// A verified overlap: the inlier correspondences (ax, ay, bx, by) in full-resolution pixels.
struct Link {
    a: usize,
    b: usize,
    matches: usize,
    pts: Vec<[f32; 4]>,
    learned: bool,
}

fn find_root(c: &mut [usize], i: usize) -> usize {
    let mut r = i;
    while c[r] != r {
        r = c[r];
    }
    let mut j = i;
    while c[j] != r {
        let next = c[j];
        c[j] = r;
        j = next;
    }
    r
}

/// Connected-component label of every image.
fn components(n: usize, edges: impl Iterator<Item = (usize, usize)>) -> Vec<usize> {
    let mut comp: Vec<usize> = (0..n).collect();
    for (a, b) in edges {
        let (ra, rb) = (find_root(&mut comp, a), find_root(&mut comp, b));
        comp[ra] = rb;
    }
    (0..n).map(|i| find_root(&mut comp, i)).collect()
}

fn count_groups(comp: &[usize]) -> usize {
    comp.iter().enumerate().filter(|(i, c)| *i == **c).count()
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
    let models: Vec<LensModel> =
        project.lenses.iter().enumerate().map(|(i, l)| if i == lens_idx { cand.model() } else { l.model() }).collect();
    pairs
        .par_iter()
        .map(|p| {
            let (ma, mb) = (&models[project.images[p.a].lens], &models[project.images[p.b].lens]);
            pair_inliers(features, p, ma, mb, tol_px).map(|f| f.inliers.len()).unwrap_or(0)
        })
        .sum()
}

fn estimate_lens(
    features: &[FeatureSet],
    pairs: &[Pair],
    project: &Project,
    lens_idx: usize,
    kinds: &[LensKind],
    tol_px: f64,
) -> Option<(Lens, usize)> {
    let involved: Vec<&Pair> =
        pairs.iter().filter(|p| project.images[p.a].lens == lens_idx || project.images[p.b].lens == lens_idx).collect();
    if involved.is_empty() {
        return None;
    }
    let base = project.lenses[lens_idx];
    let mut best: Option<(Lens, usize)> = None;
    let mut best_rect = 0usize;
    for &kind in kinds {
        let grid: &[f64] = if kind.is_fisheye() {
            &[90.0, 105.0, 120.0, 140.0, 160.0, 180.0, 200.0]
        } else {
            &[10.0, 14.0, 19.0, 25.0, 32.0, 40.0, 49.0, 58.0, 67.0, 77.0, 88.0, 100.0, 112.0]
        };
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
                if !(4.0..=300.0).contains(&cand) || (!kind.is_fisheye() && cand > 150.0) {
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

/// Learned matching between photos the SIFT pass left in separate groups. Tries capture-order
/// neighbours and pairs SIFT half-matched first, then the pairs whose learned descriptors agree
/// most, and keeps every pair a rotation explains.
fn rescue_pass(
    project: &Project,
    models: &[LensModel],
    links: &mut Vec<Link>,
    rescue: &dyn Rescue,
    quick: &[Pair],
    report: &mut AlignReport,
    progress: Progress,
) {
    let n = project.images.len();
    // Capture order: EXIF time, then file name.
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|&a, &b| {
        let (ia, ib) = (&project.images[a], &project.images[b]);
        (ia.exif.taken.as_deref().unwrap_or(""), &ia.name, a).cmp(&(ib.exif.taken.as_deref().unwrap_or(""), &ib.name, b))
    });
    let mut pos = vec![0usize; n];
    for (k, &i) in order.iter().enumerate() {
        pos[i] = k;
    }
    let groups_before = count_groups(&components(n, links.iter().map(|l| (l.a, l.b))));
    let mut tried: HashSet<(usize, usize)> = links.iter().map(|l| (l.a, l.b)).collect();
    let mut affinity: HashMap<(usize, usize), f32> = HashMap::new();
    let (mut attempted, mut added) = (0usize, 0usize);
    for round in 0..4 {
        let comp = components(n, links.iter().map(|l| (l.a, l.b)));
        if count_groups(&comp) == 1 {
            break;
        }
        let mut cands: Vec<(usize, usize)> = Vec::new();
        let key = |a: usize, b: usize| (a.min(b), a.max(b));
        if round == 0 {
            for i in 0..n {
                for d in 1..=2usize {
                    for k in [pos[i].checked_sub(d), Some(pos[i] + d)].into_iter().flatten() {
                        if let Some(&j) = order.get(k) {
                            if comp[i] != comp[j] {
                                cands.push(key(i, j));
                            }
                        }
                    }
                }
            }
            let mut hints: Vec<&Pair> = quick.iter().filter(|p| comp[p.a] != comp[p.b] && p.raw.len() >= 6).collect();
            hints.sort_by_key(|p| std::cmp::Reverse(p.raw.len()));
            cands.extend(hints.iter().take(2 * n).map(|p| key(p.a, p.b)));
        } else {
            let cross: Vec<(usize, usize)> = (0..n)
                .flat_map(|a| (a + 1..n).map(move |b| (a, b)))
                .filter(|&(a, b)| comp[a] != comp[b] && !tried.contains(&(a, b)) && !affinity.contains_key(&(a, b)))
                .collect();
            if !cross.is_empty() {
                match rescue.affinity(&cross, &|f, s| progress(0.5 * f, s)) {
                    Ok(v) => affinity.extend(cross.iter().copied().zip(v)),
                    Err(e) => {
                        report.log.push(format!("Learned matching unavailable: {e:#}"));
                        return;
                    }
                }
            }
            // The best few untried partners of every photo outside the largest group, and the best
            // links between every two groups.
            let mut size: HashMap<usize, usize> = HashMap::new();
            for &c in &comp {
                *size.entry(c).or_default() += 1;
            }
            let main = *size.iter().max_by_key(|(c, s)| (**s, usize::MAX - **c)).unwrap().0;
            let mut ranked: Vec<((usize, usize), f32)> =
                affinity.iter().filter(|(p, &v)| comp[p.0] != comp[p.1] && !tried.contains(p) && v >= 8.0).map(|(p, v)| (*p, *v)).collect();
            ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
            let mut per_image = vec![0usize; n];
            let mut per_groups: HashMap<(usize, usize), usize> = HashMap::new();
            for &((a, b), _) in &ranked {
                let g = (comp[a].min(comp[b]), comp[a].max(comp[b]));
                let gi = per_groups.entry(g).or_default();
                let outside = |i: usize| comp[i] != main && per_image[i] < 3;
                if *gi < 3 || outside(a) || outside(b) {
                    cands.push((a, b));
                    *gi += 1;
                    per_image[a] += 1;
                    per_image[b] += 1;
                }
            }
        }
        cands.sort_unstable();
        cands.dedup();
        cands.retain(|p| !tried.contains(p));
        cands.truncate(4 * n);
        if cands.is_empty() {
            if round == 0 {
                continue;
            }
            break;
        }
        tried.extend(cands.iter().copied());
        attempted += cands.len();
        let new = rescue_match(project, models, rescue, &cands, None, report, &|f, s| progress(0.5 + 0.5 * f, s));
        added += new.len();
        links.extend(new);
    }
    links.sort_by_key(|l| (l.a, l.b));
    let groups_after = count_groups(&components(n, links.iter().map(|l| (l.a, l.b))));
    report.log.push(format!(
        "{}: {added} of {attempted} tried pairs verified; groups of connected photos {groups_before} → {groups_after}",
        rescue.name()
    ));
}

/// Share of each photo's frame the other is predicted to cover (the smaller of the two).
fn predicted_overlap(ma: &LensModel, mb: &LensModel, ra: &Rotation3<f64>, rb: &Rotation3<f64>) -> f64 {
    let frac = |m1: &LensModel, m2: &LensModel, r12: Rotation3<f64>| {
        const G: usize = 9;
        let mut inside = 0;
        for gy in 0..G {
            for gx in 0..G {
                let (x, y) = ((gx as f64 + 0.5) / G as f64 * m1.w, (gy as f64 + 0.5) / G as f64 * m1.h);
                if let Some((px, py)) = m2.ray_to_pixel(r12 * m1.pixel_to_ray(x, y)) {
                    if m2.contains(px, py) {
                        inside += 1;
                    }
                }
            }
        }
        inside as f64 / (G * G) as f64
    };
    frac(ma, mb, rb.inverse() * ra).min(frac(mb, ma, ra.inverse() * rb))
}

/// Matches pairs with the rescue matcher, fast tier first and the thorough tier for the pairs the
/// fast one could not verify, and returns the verified links. `expect` gives the relative rotation
/// the current layout predicts (camera b to camera a), which a link must agree with.
fn rescue_match(
    project: &Project,
    models: &[LensModel],
    rescue: &dyn Rescue,
    pairs: &[(usize, usize)],
    expect: Option<&(dyn Fn(usize, usize) -> Option<Rotation3<f64>> + Sync)>,
    report: &mut AlignReport,
    progress: Progress,
) -> Vec<Link> {
    let verify = |(a, b): (usize, usize), m: &RescueMatches| -> Option<Link> {
        if m.pts.len() < 12 {
            return None;
        }
        let (ma, mb) = (&models[project.images[a].lens], &models[project.images[b].lens]);
        let rays: Vec<(V3, V3)> =
            m.pts.iter().map(|p| (ma.pixel_to_ray(p[0] as f64, p[1] as f64), mb.pixel_to_ray(p[2] as f64, p[3] as f64))).collect();
        // Learned keypoints are localised to about a pixel of the image the network saw; the lens may
        // still be rough, so allow a small fraction of the frame as well.
        let long = project.images[a].width.max(project.images[a].height) as f64;
        let tol_px = (3.0 * m.sigma as f64).max(0.006 * long);
        let fit = ransac_rotation(&rays, tol_px / ma.f.min(mb.f), (a * 7919 + b) as u64 ^ 0x5eed)?;
        if std::env::var_os("NODAL_DEBUG_RESCUE").is_some() {
            eprintln!("rescue {}-{}: {} matches, {} inliers (tol {tol_px:.1} px)", a + 1, b + 1, m.pts.len(), fit.inliers.len());
        }
        if fit.inliers.len() < 12 || fit.inliers.len() as f64 <= 6.0 + 0.22 * m.pts.len() as f64 {
            return None;
        }
        // The inliers must cover some of the frame: a cluster on one small patch fixes nothing.
        let (w, h) = (project.images[a].width as f32, project.images[a].height as f32);
        let mut cells = [false; 64];
        for &k in &fit.inliers {
            let p = m.pts[k];
            let (cx, cy) = (((p[0] / w * 8.0) as usize).min(7), ((p[1] / h * 8.0) as usize).min(7));
            cells[cy * 8 + cx] = true;
        }
        if cells.iter().filter(|&&c| c).count() < 4 {
            return None;
        }
        if let Some(r) = expect.and_then(|e| e(a, b)) {
            if crate::global::rot_angle(&(fit.r.inverse() * r)) > 8f64.to_radians() {
                return None;
            }
        }
        Some(Link { a, b, matches: m.pts.len(), pts: fit.inliers.iter().map(|&k| m.pts[k]).collect(), learned: true })
    };
    let fast = match rescue.correspondences(pairs, false, &|f, s| progress(0.4 * f, s)) {
        Ok(v) => v,
        Err(e) => {
            report.log.push(format!("Learned matching unavailable: {e:#}"));
            return Vec::new();
        }
    };
    let results: Vec<Option<Link>> = pairs.par_iter().zip(fast.par_iter()).map(|(&p, m)| verify(p, m)).collect();
    let failed: Vec<(usize, usize)> = pairs.iter().zip(&results).filter(|(_, r)| r.is_none()).map(|(p, _)| *p).collect();
    let mut out: Vec<Link> = results.into_iter().flatten().collect();
    if !failed.is_empty() {
        match rescue.correspondences(&failed, true, &|f, s| progress(0.4 + 0.6 * f, s)) {
            Ok(v) => out.extend(failed.par_iter().zip(v.par_iter()).filter_map(|(&p, m)| verify(p, m)).collect::<Vec<_>>()),
            Err(e) => report.log.push(format!("Learned matching unavailable: {e:#}")),
        }
    }
    out
}

/// Whether the placed photos go all the way round: their viewing directions, measured about the
/// panorama's own vertical (the normal of the plane the cameras' horizontal axes span, as in
/// [`level`]), leave no gap wider than 100°. Done in that frame because the reference photo may
/// itself point steeply up or down.
fn ring_closes(rots: &[Option<Rotation3<f64>>]) -> bool {
    let placed: Vec<&Rotation3<f64>> = rots.iter().flatten().collect();
    if placed.len() < 4 {
        return false;
    }
    let mut c = Matrix3::<f64>::zeros();
    for r in &placed {
        let x: V3 = *r * V3::x();
        c += x * x.transpose();
    }
    let eig = SymmetricEigen::new(c);
    let k = (0..3).min_by(|&a, &b| eig.eigenvalues[a].total_cmp(&eig.eigenvalues[b])).unwrap();
    let up: V3 = eig.eigenvectors.column(k).into_owned().normalize();
    let e1 = if up.x.abs() < 0.9 { up.cross(&V3::x()) } else { up.cross(&V3::y()) }.normalize();
    let e2 = up.cross(&e1);
    let mut yaws: Vec<f64> =
        placed.iter().map(|r| *r * V3::z()).filter(|d| d.dot(&up).abs() < 0.85).map(|d| d.dot(&e2).atan2(d.dot(&e1))).collect();
    if yaws.len() < 4 {
        return false;
    }
    yaws.sort_by(|a, b| a.total_cmp(b));
    let mut gap = yaws[0] + std::f64::consts::TAU - yaws[yaws.len() - 1];
    for w in yaws.windows(2) {
        gap = gap.max(w[1] - w[0]);
    }
    gap < 100f64.to_radians()
}

/// Where every photo points, from the verified links alone.
struct Placement {
    anchor: usize,
    rots: Vec<Option<Rotation3<f64>>>,
    edges: Vec<crate::global::Edge>,
    keep_edge: Vec<bool>,
    residuals: Vec<f64>,
    log: Vec<String>,
}

/// Global placement: robust rotation averaging over every link, with a focal-length search by global
/// consistency (a ring only closes at the right focal length) and removal of links that contradict
/// the cycles they belong to.
fn place(project: &mut Project, links: &[Link], opts: &AlignOptions, searched: &mut [bool]) -> Placement {
    let n = project.images.len();
    let mut log = Vec::new();
    let mut degree = vec![0usize; n];
    for l in links {
        degree[l.a] += l.pts.len();
        degree[l.b] += l.pts.len();
    }
    let comp0 = components(n, links.iter().map(|l| (l.a, l.b)));
    let mut comp_weight = HashMap::<usize, usize>::new();
    for l in links {
        *comp_weight.entry(comp0[l.a]).or_default() += l.pts.len();
    }
    let main = *comp_weight.iter().max_by_key(|(c, w)| (**w, usize::MAX - **c)).unwrap().0;
    let anchor = (0..n).filter(|&i| comp0[i] == main).max_by_key(|&i| (degree[i], usize::MAX - i)).unwrap();
    let pair_points: Vec<crate::global::PairPoints> = links
        .iter()
        .map(|l| crate::global::PairPoints { a: l.a, b: l.b, pts: l.pts.iter().map(|p| (p[0], p[1], p[2], p[3])).collect() })
        .collect();
    let lens_idx: Vec<usize> = project.images.iter().map(|i| i.lens).collect();
    if opts.lens.is_none() && opts.optimize_lens {
        for li in 0..project.lenses.len() {
            if !lens_idx.contains(&li) {
                continue;
            }
            let base = project.lenses[li];
            let f0 = base.focal_px();
            let eval_full = |f: f64| {
                let mut models: Vec<LensModel> = project.lenses.iter().map(|l| l.model()).collect();
                let mut l = base;
                l.set_focal_px(f);
                models[li] = l.model();
                crate::global::consistency_cost(n, &pair_points, &models, &lens_idx, anchor)
            };
            let eval = |f: f64| eval_full(f).0;
            // The focal length is only well determined by global consistency when the photos close
            // a ring: elsewhere it trades off against the spacing of the shots. A badly wrong start
            // can tear the ring open (pairs then disagree around it), so look for one over a range.
            let is_ring = |f: f64| ring_closes(&eval_full(f).1);
            let ring = [1.0, 1.3, 0.77, 1.6].iter().any(|&s| is_ring(f0 * s));
            if !ring {
                continue;
            }
            // Coarse logarithmic grid, ±40 %, then golden-section refinement.
            let grid: Vec<f64> = (0..=28).map(|k| f0 * (0.7f64.ln() + (1.45f64.ln() - 0.7f64.ln()) * k as f64 / 28.0).exp()).collect();
            let costs: Vec<f64> = grid.par_iter().map(|&f| eval(f)).collect();
            let (kbest, _) = costs.iter().enumerate().min_by(|a, b| a.1.total_cmp(b.1)).unwrap();
            let (mut lo, mut hi) = (grid[kbest.saturating_sub(1)], grid[(kbest + 1).min(grid.len() - 1)]);
            let g = 0.5 * (5f64.sqrt() - 1.0);
            let (mut x1, mut x2) = (hi - g * (hi - lo), lo + g * (hi - lo));
            let (mut c1, mut c2) = (eval(x1), eval(x2));
            for _ in 0..14 {
                if c1 < c2 {
                    hi = x2;
                    x2 = x1;
                    c2 = c1;
                    x1 = hi - g * (hi - lo);
                    c1 = eval(x1);
                } else {
                    lo = x1;
                    x1 = x2;
                    c1 = c2;
                    x2 = lo + g * (hi - lo);
                    c2 = eval(x2);
                }
            }
            let f_best = 0.5 * (lo + hi);
            // Only trust a clear optimum (a flat cost means the focal length is unobservable).
            let (c_best, rots_best) = eval_full(f_best);
            let c_far = costs.iter().copied().fold(f64::NEG_INFINITY, f64::max);
            // Judge the optimum on the mean reprojection cost alone (photos outside the group add
            // a constant that would otherwise swamp the comparison).
            let missing = rots_best.iter().filter(|r| r.is_none()).count() as f64 * crate::global::MISSING_COST;
            let (c_best, c_far) = (c_best - missing, c_far - missing);
            if c_far > c_best * 1.05 {
                let before = base.hfov;
                project.lenses[li].set_focal_px(f_best);
                searched[li] = true;
                log.push(format!(
                    "Lens {}: focal length {:.0} px ({:.1}° wide) from global consistency (started at {:.1}°)",
                    li + 1,
                    f_best,
                    project.lenses[li].hfov,
                    before
                ));
            }
        }
    }
    let models: Vec<LensModel> = project.lenses.iter().map(|l| l.model()).collect();
    let learned: Vec<bool> = links.iter().map(|l| l.learned).collect();
    let edges: Vec<crate::global::Edge> = pair_points
        .iter()
        .enumerate()
        .map(|(k, p)| {
            let (ma, mb) = (&models[lens_idx[p.a]], &models[lens_idx[p.b]]);
            let rays: Vec<(V3, V3)> = p
                .pts
                .iter()
                .map(|&(ax, ay, bx, by)| (ma.pixel_to_ray(ax as f64, ay as f64), mb.pixel_to_ray(bx as f64, by as f64)))
                .collect();
            let r = crate::matching::kabsch(rays.iter().copied()).unwrap_or_else(Rotation3::identity);
            // Learned links bridge groups; where they close cycles with SIFT links, SIFT decides.
            let trust = if learned[k] { 0.4 } else { 1.0 };
            crate::global::Edge { a: p.a, b: p.b, r, w: trust * (p.pts.len() as f64).sqrt() }
        })
        .collect();
    let (rots, keep_edge, residuals) = crate::global::average_robust(n, &edges, anchor);
    Placement { anchor, rots, edges, keep_edge, residuals, log }
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
/// A pair that is the only link between two groups of photos keeps its best points even when they
/// are noisier than the rest (learned matches, strong parallax): pruning never splits the panorama.
fn prune(project: &mut Project, floor_px: f32, sigmas: f32) -> usize {
    optimize::update_errors(project);
    let med = median(project.control_points.iter().map(|c| c.error).collect());
    let thr = (med * sigmas).max(floor_px);
    let before = project.control_points.len();
    let (mut keep, dropped): (Vec<ControlPoint>, Vec<ControlPoint>) =
        std::mem::take(&mut project.control_points).into_iter().partition(|c| c.error <= thr);
    if !dropped.is_empty() {
        let n = project.images.len();
        let mut kept_per_pair: HashMap<(u32, u32), usize> = HashMap::new();
        for c in &keep {
            *kept_per_pair.entry((c.a, c.b)).or_default() += 1;
        }
        let mut comp = components(n, kept_per_pair.iter().filter(|(_, &k)| k >= 3).map(|(p, _)| (p.0 as usize, p.1 as usize)));
        let mut by_pair: HashMap<(u32, u32), Vec<ControlPoint>> = HashMap::new();
        for c in dropped {
            by_pair.entry((c.a, c.b)).or_default().push(c);
        }
        let mut pairs: Vec<((u32, u32), Vec<ControlPoint>)> = by_pair.into_iter().collect();
        for (_, v) in &mut pairs {
            v.sort_by(|a, b| a.error.total_cmp(&b.error));
        }
        pairs.sort_by(|a, b| a.1[0].error.total_cmp(&b.1[0].error).then(a.0.cmp(&b.0)));
        for ((a, b), pts) in pairs {
            let kept = kept_per_pair.get(&(a, b)).copied().unwrap_or(0);
            let (ra, rb) = (find_root(&mut comp, a as usize), find_root(&mut comp, b as usize));
            if ra == rb || kept + pts.len() < 6 {
                continue;
            }
            let restore: Vec<ControlPoint> = pts.into_iter().take(8usize.saturating_sub(kept)).collect();
            // A gross outlier pair (wrong match) stays dropped.
            if median(restore.iter().map(|c| c.error).collect()) > thr * 8.0 {
                continue;
            }
            keep.extend(restore);
            comp[ra] = rb;
        }
    }
    project.control_points = keep;
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
        if let Some(f35) = exif.focal_35mm {
            // Focal length known but the mapping is ambiguous (very wide lens): test both mappings at that focal length.
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
    let comp = components(n, verified.iter().map(|(p, _)| (p.a, p.b)));
    if count_groups(&comp) > 1 {
        progress(0.45, "Searching for more overlaps");
        let tried: HashSet<(usize, usize)> = promising.iter().copied().collect();
        let extra: Vec<(usize, usize)> =
            all_pairs.iter().copied().filter(|&(a, b)| comp[a] != comp[b] && !tried.contains(&(a, b))).collect();
        verified.extend(extra.par_iter().filter_map(|&(a, b)| verify(a, b)).collect::<Vec<_>>());
    }
    verified.sort_by_key(|(p, _)| (p.a, p.b));
    let mut links: Vec<Link> = verified
        .iter()
        .map(|(p, fit)| Link {
            a: p.a,
            b: p.b,
            matches: p.raw.len(),
            learned: false,
            pts: fit
                .inliers
                .iter()
                .map(|&k| {
                    let m = p.raw[k];
                    let (fa, fb) = (&features[p.a].feats[m.a as usize], &features[p.b].feats[m.b as usize]);
                    [fa.x, fa.y, fb.x, fb.y]
                })
                .collect(),
        })
        .collect();
    drop(verified);

    // 3b. Photos SIFT could not connect: learned features, matched across the separate groups.
    if let Some(rescue) = &opts.rescue {
        if count_groups(&components(n, links.iter().map(|l| (l.a, l.b)))) > 1 {
            progress(0.46, "Learned matching");
            rescue_pass(project, &models, &mut links, rescue.0.as_ref(), &quick, &mut report, &|f, s| progress(0.46 + 0.04 * f, s));
        }
    }
    for l in &links {
        report.pairs.push(PairStat { a: l.a, b: l.b, matches: l.matches, inliers: l.pts.len(), learned: l.learned });
    }
    anyhow::ensure!(
        !links.is_empty(),
        "No overlapping images were found. Check that the photos overlap by at least 20–30 % and that the lens type is right."
    );

    // 4. Global placement.
    progress(0.5, "Placing images");
    let mut searched = vec![false; project.lenses.len()];
    let mut placed = place(project, &links, opts, &mut searched);
    // 4b. With the layout known, weakly tied photos get learned matching against every photo they
    //     are now predicted to overlap: closing rings and cross-links is what makes the layout rigid.
    if let Some(rescue) = &opts.rescue {
        let mut strength = vec![0usize; n];
        for l in &links {
            strength[l.a] += l.pts.len();
            strength[l.b] += l.pts.len();
        }
        let weak = (0..n).any(|i| placed.rots[i].is_some() && strength[i] < 60);
        if weak || links.iter().any(|l| l.learned) {
            let linked: HashSet<(usize, usize)> = links.iter().map(|l| (l.a, l.b)).collect();
            let models: Vec<LensModel> = project.lenses.iter().map(|l| l.model()).collect();
            let mut cands: Vec<((usize, usize), f64)> = (0..n)
                .flat_map(|a| (a + 1..n).map(move |b| (a, b)))
                .filter(|p| !linked.contains(p))
                .filter_map(|(a, b)| {
                    let (ra, rb) = (placed.rots[a]?, placed.rots[b]?);
                    let ov = predicted_overlap(&models[project.images[a].lens], &models[project.images[b].lens], &ra, &rb);
                    (ov >= 0.12).then_some(((a, b), ov))
                })
                .collect();
            cands.sort_by(|x, y| y.1.total_cmp(&x.1));
            cands.truncate(6 * n);
            if !cands.is_empty() {
                progress(0.5, "Learned matching of predicted overlaps");
                let pairs: Vec<(usize, usize)> = cands.iter().map(|c| c.0).collect();
                let rots = placed.rots.clone();
                let expect = move |a: usize, b: usize| rots[a].zip(rots[b]).map(|(ra, rb)| ra.inverse() * rb);
                let new = rescue_match(project, &models, rescue.0.as_ref(), &pairs, Some(&expect), &mut report, &|f, s| {
                    progress(0.5 + 0.05 * f, s)
                });
                if !new.is_empty() {
                    report.log.push(format!("Learned matching of predicted overlaps: {} of {} pairs verified", new.len(), pairs.len()));
                    for l in &new {
                        report.pairs.push(PairStat { a: l.a, b: l.b, matches: l.matches, inliers: l.pts.len(), learned: true });
                    }
                    links.extend(new);
                    links.sort_by_key(|l| (l.a, l.b));
                    placed = place(project, &links, opts, &mut searched);
                }
            }
        }
    }
    report.log.append(&mut placed.log);
    let Placement { anchor, rots, edges, keep_edge, residuals, .. } = placed;
    let dropped: Vec<String> = edges
        .iter()
        .zip(&keep_edge)
        .zip(&residuals)
        .filter(|((_, k), _)| !**k)
        .map(|((e, _), r)| format!("{}–{} ({:.1}°)", e.a + 1, e.b + 1, r.to_degrees()))
        .collect();
    if !dropped.is_empty() {
        report.log.push(format!("Dropped {} inconsistent pair(s): {}", dropped.len(), dropped.join(", ")));
    }
    let mut k = 0;
    links.retain(|_| {
        let keep = keep_edge[k];
        k += 1;
        keep
    });
    report.pairs.retain(|ps| links.iter().any(|l| l.a == ps.a && l.b == ps.b));
    for i in 0..n {
        if let Some(r) = rots[i] {
            project.images[i].set_rot(&r);
            project.images[i].aligned = true;
        } else {
            report.unaligned.push(i);
        }
    }

    // 5. Control points from the verified matches.
    for l in &links {
        if !(project.images[l.a].aligned && project.images[l.b].aligned) {
            continue;
        }
        let pts: Vec<ControlPoint> = l
            .pts
            .iter()
            .map(|p| ControlPoint { a: l.a as u32, b: l.b as u32, ax: p[0], ay: p[1], bx: p[2], by: p[3], error: 0.0 })
            .collect();
        let (w, h) = (project.images[l.a].width as f32, project.images[l.a].height as f32);
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
        .zip(&searched)
        .map(|((l, src), &found)| match src {
            LensSource::User => Some((l.hfov, (l.hfov * 0.01).max(0.1))),
            _ if found => Some((l.hfov, (l.hfov * 0.05).max(0.5))),
            // Zoom lenses often differ from their EXIF focal length by 5–10 %.
            LensSource::Exif => Some((l.hfov, (l.hfov * 0.12).max(1.0))),
            LensSource::Estimated => Some((l.hfov, l.hfov * 0.5)),
        })
        .collect();
    let run = |project: &mut Project, flags: LensFlags, huber: f64| {
        let cps = project.control_points.clone();
        optimize::optimize(
            project,
            &cps,
            &OptOptions { flags, huber_px: huber, max_iters: 60, fov_prior: fov_prior.clone(), fixed: fixed.clone() },
        )
    };
    let floor = (1.5 * px_mean) as f32;
    let r = run(project, LensFlags::NONE, 3.0 * px_mean);
    report.log.push(format!("Orientations: {} points, RMS {:.2} px", project.control_points.len(), r.rms));
    prune(project, floor * 4.0, 4.0);
    let user_lens = opts.lens.is_some();
    // Lens parameters are added in stages, each kept only if it predicts control points it was not
    // fitted to (every fifth point of each pair is held out): a parameter that merely absorbs noise
    // makes the frame edges worse, where seams live, while lowering the fitted error.
    let mut k_on = false;
    if opts.optimize_lens {
        let per_lens = project.control_points.len() / project.lenses.len().max(1);
        let fisheye = project.lenses.iter().any(|l| l.is_fisheye());
        let none = LensFlags::NONE;
        let stages: [(LensFlags, usize, &str, bool); 5] = [
            (LensFlags { fov: !user_lens, ..none }, 12, "field of view", true),
            // PTGui's advice: fit the projection with the polynomial off, then add the polynomial.
            (LensFlags { fov: !user_lens, k: true, ..none }, 30, "projection", fisheye && !user_lens),
            (LensFlags { fov: !user_lens, b: true, ..none }, 30, "distortion b", true),
            (LensFlags { fov: !user_lens, a: true, b: true, c: true, ..none }, 80, "distortion a, b, c", true),
            (LensFlags { fov: !user_lens, a: true, b: true, c: true, shift: true, ..none }, 150, "lens shift", true),
        ];
        for (i, (flags, need, label, enabled)) in stages.iter().enumerate() {
            let flags = LensFlags { k: flags.k || k_on, ..*flags };
            if !enabled || per_lens < *need || flags == LensFlags::NONE {
                continue;
            }
            progress(0.6 + 0.06 * (i + 1) as f32, "Calibrating lens");
            let (train, held) = split_held_out(&project.control_points);
            let before = held_out_error(project, &held);
            let backup = project.clone();
            optimize::optimize(
                project,
                &train,
                &OptOptions { flags, huber_px: 2.5 * px_mean, max_iters: 60, fov_prior: fov_prior.clone(), fixed: fixed.clone() },
            );
            let after = held_out_error(project, &held);
            if !lenses_sane(project) || after > before * 0.997 {
                *project = backup;
                continue;
            }
            if flags.k && !k_on {
                k_on = true;
                let ks: Vec<String> = project.lenses.iter().filter(|l| l.is_fisheye()).map(|l| l.projection_label()).collect();
                report.log.push(format!("Calibrated projection: {} (held-out error {before:.2} → {after:.2} px)", ks.join(", ")));
            } else {
                report.log.push(format!("Calibrated {label}: held-out error {before:.2} → {after:.2} px"));
            }
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
        let flags = if opts.optimize_lens {
            LensFlags {
                fov: !user_lens,
                a: true,
                b: true,
                c: true,
                shift: project.control_points.len() / project.lenses.len().max(1) >= 150,
                k: k_on,
            }
        } else {
            LensFlags::NONE
        };
        let backup = project.clone();
        let r = run(project, flags, 2.0 * px_mean);
        if !lenses_sane(project) {
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

/// Lens parameters still describe a plausible lens.
fn lenses_sane(project: &Project) -> bool {
    project.lenses.iter().all(|l| l.hfov.is_finite() && l.hfov > 1.0 && l.k().is_finite() && (l.a.abs() + l.b.abs() + l.c.abs()) < 0.9)
}

/// Every fifth control point of each pair is held out to judge whether a new lens parameter
/// predicts, rather than just fits.
fn split_held_out(cps: &[ControlPoint]) -> (Vec<ControlPoint>, Vec<ControlPoint>) {
    let mut count: HashMap<(u32, u32), usize> = HashMap::new();
    let (mut train, mut held) = (Vec::with_capacity(cps.len()), Vec::with_capacity(cps.len() / 5 + 1));
    for c in cps {
        let k = count.entry((c.a, c.b)).or_default();
        if *k % 5 == 2 {
            held.push(*c);
        } else {
            train.push(*c);
        }
        *k += 1;
    }
    (train, held)
}

/// RMS distance of the best 90 % of held-out control points (a few mismatches should not decide).
fn held_out_error(project: &Project, held: &[ControlPoint]) -> f64 {
    let models: Vec<LensModel> = project.lenses.iter().map(|l| l.model()).collect();
    let mut e: Vec<f64> = held
        .iter()
        .map(|c| {
            let (a, b) = (c.a as usize, c.b as usize);
            optimize::cp_error(
                c,
                &project.images[a].rot(),
                &project.images[b].rot(),
                &models[project.images[a].lens],
                &models[project.images[b].lens],
            )
        })
        .collect();
    if e.is_empty() {
        return 0.0;
    }
    e.sort_by(|a, b| a.total_cmp(b));
    let keep = &e[..(e.len() * 9 / 10).max(1)];
    (keep.iter().map(|x| x * x).sum::<f64>() / keep.len() as f64).sqrt()
}

/// Final optimisation pass after control points have been refined: tight robust threshold, all
/// lens parameters the data supports, outliers dropped. Returns (RMS, max) in source pixels.
pub fn polish(project: &mut Project, opts: &AlignOptions) -> (f64, f64) {
    let n = project.images.len();
    let anchor = (0..n).find(|&i| project.images[i].aligned).unwrap_or(0);
    let mut fixed = vec![false; n];
    fixed[anchor] = true;
    for (i, img) in project.images.iter().enumerate() {
        if !img.aligned {
            fixed[i] = true;
        }
    }
    let fov_prior: Vec<Option<(f64, f64)>> = project.lenses.iter().map(|l| Some((l.hfov, (l.hfov * 0.02).max(0.2)))).collect();
    let per_lens = project.control_points.len() / project.lenses.len().max(1);
    let user_lens = opts.lens.is_some();
    // The projection factor keeps moving only if alignment found it worth fitting.
    let k = project.lenses.iter().any(|l| l.k.is_some() && l.is_fisheye());
    let flags = if opts.optimize_lens && per_lens >= 30 {
        LensFlags { fov: !user_lens, a: per_lens >= 80, b: true, c: per_lens >= 80, shift: per_lens >= 150, k: k && !user_lens }
    } else {
        LensFlags::NONE
    };
    for round in 0..3 {
        let backup = project.clone();
        let cps = project.control_points.clone();
        // Robust scale from the data itself.
        optimize::update_errors(project);
        let med = median(project.control_points.iter().map(|c| c.error).collect()).max(0.15) as f64;
        optimize::optimize(
            project,
            &cps,
            &OptOptions { flags, huber_px: 2.5 * med, max_iters: 60, fov_prior: fov_prior.clone(), fixed: fixed.clone() },
        );
        if !lenses_sane(project) {
            *project = backup;
            break;
        }
        if round < 2 && prune(project, 0.6, 4.0) == 0 {
            break;
        }
    }
    optimize::update_errors(project);
    let lens_idx: Vec<usize> = project.images.iter().map(|i| i.lens).collect();
    optimize::error_stats(project, &project.control_points, &lens_idx)
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
    // Per pair, keep whichever set is larger: guided SIFT can find fewer points than the pair already
    // has (low texture, learned matches), and must not cut a photo loose.
    let mut existing: HashMap<(u32, u32), Vec<ControlPoint>> = HashMap::new();
    for c in &project.control_points {
        existing.entry((c.a, c.b)).or_default().push(*c);
    }
    let before = project.control_points.len();
    let mut out: Vec<ControlPoint> = Vec::new();
    let mut gained = false;
    for (&(a, b), pts) in pairs.iter().zip(found) {
        let old = existing.remove(&(a as u32, b as u32)).unwrap_or_default();
        if pts.len() > old.len() {
            gained = true;
            out.extend(pts);
        } else {
            out.extend(old);
        }
    }
    out.extend(existing.into_values().flatten());
    if !gained {
        return 0;
    }
    out.sort_by_key(|c| (c.a, c.b));
    project.control_points = out;
    project.control_points.len().saturating_sub(before).max(1)
}

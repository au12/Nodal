//! Global placement: robust rotation averaging over the verified pair graph, cycle-consistency
//! outlier removal, and focal-length search by global consistency.
//!
//! A spanning tree chains pairwise rotations and so accumulates every error along its branches;
//! a 360° ring then fails to close and a wrong focal length goes unnoticed. Averaging uses every
//! verified pair at once: a wrong pair (repeating texture, a moving crowd) shows up as an edge that
//! disagrees with the cycles it belongs to and is removed, and a wrong focal length shows up as a
//! ring that cannot close, so the focal length that makes the whole graph most consistent is a
//! robust estimate even when parallax makes individual pairs noisy.

use crate::lens::{LensModel, V3};
use nalgebra::{DMatrix, Matrix3, Rotation3, SymmetricEigen};

/// A verified pair: `r` maps rays of image `b` into image `a`'s frame, i.e. `R_b = R_a · r` for
/// camera → world rotations.
#[derive(Clone, Debug)]
pub struct Edge {
    pub a: usize,
    pub b: usize,
    pub r: Rotation3<f64>,
    /// Confidence (e.g. √inliers).
    pub w: f64,
}

/// Rotation angle, robust near 0 and π (nalgebra's `angle()` uses `acos` of the trace and can
/// return NaN through rounding).
#[inline]
pub fn rot_angle(r: &Rotation3<f64>) -> f64 {
    let m = r.matrix();
    let s = 0.5 * V3::new(m[(2, 1)] - m[(1, 2)], m[(0, 2)] - m[(2, 0)], m[(1, 0)] - m[(0, 1)]).norm();
    let c = 0.5 * (m.trace() - 1.0);
    s.atan2(c)
}

/// Angle between two directions, robust for tiny angles.
#[inline]
pub fn vec_angle(a: &V3, b: &V3) -> f64 {
    a.cross(b).norm().atan2(a.dot(b))
}

/// Nearest rotation to a 3×3 matrix (polar decomposition).
pub fn project_so3(m: &Matrix3<f64>) -> Rotation3<f64> {
    let svd = m.svd(true, true);
    let (u, vt) = (svd.u.unwrap(), svd.v_t.unwrap());
    let mut d = Matrix3::identity();
    if (u * vt).determinant() < 0.0 {
        d[(2, 2)] = -1.0;
    }
    Rotation3::from_matrix_unchecked(u * d * vt)
}

/// Connected components of the graph restricted to `edges`.
pub fn components(n: usize, edges: &[Edge]) -> Vec<usize> {
    let mut parent: Vec<usize> = (0..n).collect();
    fn find(p: &mut [usize], mut i: usize) -> usize {
        while p[i] != i {
            p[i] = p[p[i]];
            i = p[i];
        }
        i
    }
    for e in edges {
        let (ra, rb) = (find(&mut parent, e.a), find(&mut parent, e.b));
        if ra != rb {
            parent[ra] = rb;
        }
    }
    (0..n).map(|i| find(&mut parent, i)).collect()
}

/// Robust rotation averaging for the component containing `anchor` (whose rotation is the
/// identity). Other images get `None`. Returns the rotations and each edge's residual angle
/// (radians; `NAN` for edges outside the component).
pub fn average(n: usize, edges: &[Edge], anchor: usize) -> (Vec<Option<Rotation3<f64>>>, Vec<f64>) {
    let comp = components(n, edges);
    let nodes: Vec<usize> = (0..n).filter(|&i| comp[i] == comp[anchor]).collect();
    let mut index = vec![usize::MAX; n];
    for (k, &i) in nodes.iter().enumerate() {
        index[i] = k;
    }
    let m = nodes.len();
    let mut rots: Vec<Option<Rotation3<f64>>> = vec![None; n];
    if m == 1 {
        rots[anchor] = Some(Rotation3::identity());
        return (rots, vec![f64::NAN; edges.len()]);
    }
    let local: Vec<&Edge> = edges.iter().filter(|e| index[e.a] != usize::MAX && index[e.b] != usize::MAX).collect();

    // Spectral initialisation: G_ab = w·R_a^T R_b = w·r_ab ≈ (M Mᵀ)_ab with M the stacked R_iᵀ.
    let mut g = DMatrix::<f64>::zeros(3 * m, 3 * m);
    let mut deg = vec![0.0f64; m];
    for e in &local {
        let (a, b) = (index[e.a], index[e.b]);
        let r = e.r.matrix();
        for i in 0..3 {
            for j in 0..3 {
                g[(3 * a + i, 3 * b + j)] += e.w * r[(i, j)];
                g[(3 * b + j, 3 * a + i)] += e.w * r[(i, j)];
            }
        }
        deg[a] += e.w;
        deg[b] += e.w;
    }
    for k in 0..m {
        for i in 0..3 {
            g[(3 * k + i, 3 * k + i)] += deg[k];
        }
    }
    let eig = SymmetricEigen::new(g);
    let mut order: Vec<usize> = (0..3 * m).collect();
    order.sort_by(|&a, &b| eig.eigenvalues[b].total_cmp(&eig.eigenvalues[a]));
    let mut est: Vec<Rotation3<f64>> = Vec::with_capacity(m);
    let mut negative = 0;
    for k in 0..m {
        let mut v = Matrix3::zeros();
        for (c, &o) in order.iter().take(3).enumerate() {
            for i in 0..3 {
                v[(i, c)] = eig.eigenvectors[(3 * k + i, o)];
            }
        }
        if v.determinant() < 0.0 {
            negative += 1;
        }
        est.push(project_so3(&v.transpose()));
    }
    if negative * 2 > m {
        // Reflection ambiguity of the eigenvectors: flip one axis and project again.
        est.clear();
        for k in 0..m {
            let mut v = Matrix3::zeros();
            for (c, &o) in order.iter().take(3).enumerate() {
                let s = if c == 2 { -1.0 } else { 1.0 };
                for i in 0..3 {
                    v[(i, c)] = s * eig.eigenvectors[(3 * k + i, o)];
                }
            }
            est.push(project_so3(&v.transpose()));
        }
    }

    // Robust refinement: Gauss–Seidel chordal means with Cauchy weights on each edge's residual.
    let adj: Vec<Vec<(usize, Rotation3<f64>, f64)>> = {
        // For node k: (neighbour, rotation that maps the neighbour's estimate to k's, weight).
        let mut adj = vec![Vec::new(); m];
        for e in &local {
            let (a, b) = (index[e.a], index[e.b]);
            // R_b = R_a r → R_a = R_b rᵀ.
            adj[b].push((a, e.r, e.w));
            adj[a].push((b, e.r.inverse(), e.w));
        }
        adj
    };
    // Graduated non-convexity: start almost quadratic (a wrong edge only bends the solution a
    // little), then tighten so it stops pulling at all.
    let mut scale = 25f64.to_radians();
    for _ in 0..80 {
        let mut moved = 0.0f64;
        for k in 0..m {
            let mut acc = Matrix3::zeros();
            for &(j, r, w) in &adj[k] {
                let cand = est[j] * r;
                let res = rot_angle(&(est[k].inverse() * cand));
                let rw = w / (1.0 + (res / scale).powi(2));
                acc += cand.matrix() * rw;
            }
            if acc.norm() < 1e-12 {
                continue;
            }
            let new = project_so3(&acc);
            moved = moved.max(rot_angle(&(est[k].inverse() * new)));
            est[k] = new;
        }
        let tight = scale <= 0.3f64.to_radians() + 1e-12;
        scale = (scale * 0.85).max(0.3f64.to_radians());
        if moved < 1e-7 && tight {
            break;
        }
    }
    // Anchor.
    let ka = index[anchor];
    let inv = est[ka].inverse();
    for (k, &i) in nodes.iter().enumerate() {
        rots[i] = Some(inv * est[k]);
    }
    let residuals = edges
        .iter()
        .map(|e| match (rots[e.a], rots[e.b]) {
            (Some(ra), Some(rb)) => rot_angle(&((ra * e.r).inverse() * rb)),
            _ => f64::NAN,
        })
        .collect();
    (rots, residuals)
}

/// Averages, then removes edges that contradict the cycles they belong to and averages again.
/// Returns rotations, the surviving edge mask and the edge residuals.
pub fn average_robust(n: usize, edges: &[Edge], anchor: usize) -> (Vec<Option<Rotation3<f64>>>, Vec<bool>, Vec<f64>) {
    let mut keep = vec![true; edges.len()];
    let mut current: Vec<Edge> = edges.to_vec();
    let mut result = average(n, &current, anchor);
    for _ in 0..3 {
        let mut res: Vec<f64> = result.1.iter().copied().filter(|r| r.is_finite()).collect();
        if res.len() < 4 {
            break;
        }
        res.sort_by(|a, b| a.total_cmp(b));
        let med = res[res.len() / 2];
        let thr = (med * 5.0).max(2.5f64.to_radians());
        let mut removed = false;
        // Only drop an edge if both images keep another connection (never cut a bridge here).
        let mut deg = vec![0usize; n];
        for (k, e) in edges.iter().enumerate() {
            if keep[k] {
                deg[e.a] += 1;
                deg[e.b] += 1;
            }
        }
        // Candidates, worst first; each is removed only if the graph stays connected without it.
        let mut cand: Vec<(usize, f64)> = Vec::new();
        let mut idx_cur = 0;
        for (k, _) in edges.iter().enumerate() {
            if !keep[k] {
                continue;
            }
            let r = result.1[idx_cur];
            idx_cur += 1;
            if r.is_finite() && r > thr {
                cand.push((k, r));
            }
        }
        cand.sort_by(|a, b| b.1.total_cmp(&a.1));
        let mut new_keep = keep.clone();
        let base_comp = components(n, &edges.iter().zip(&new_keep).filter(|(_, k)| **k).map(|(e, _)| e.clone()).collect::<Vec<_>>());
        let groups = |c: &[usize]| {
            let mut v = c.to_vec();
            v.sort();
            v.dedup();
            v.len()
        };
        let base_groups = groups(&base_comp);
        for (k, _) in cand {
            let e = &edges[k];
            if deg[e.a] <= 1 || deg[e.b] <= 1 {
                continue;
            }
            new_keep[k] = false;
            let rest: Vec<Edge> = edges.iter().zip(&new_keep).filter(|(_, k)| **k).map(|(e, _)| e.clone()).collect();
            if groups(&components(n, &rest)) > base_groups {
                new_keep[k] = true; // a bridge: keep it
                continue;
            }
            deg[e.a] -= 1;
            deg[e.b] -= 1;
            removed = true;
        }
        if !removed {
            break;
        }
        keep = new_keep;
        current = edges.iter().zip(&keep).filter(|(_, k)| **k).map(|(e, _)| e.clone()).collect();
        result = average(n, &current, anchor);
    }
    // Residuals for every original edge under the final rotations.
    let rots = result.0;
    let residuals = edges
        .iter()
        .map(|e| match (rots[e.a], rots[e.b]) {
            (Some(ra), Some(rb)) => rot_angle(&((ra * e.r).inverse() * rb)),
            _ => f64::NAN,
        })
        .collect();
    (rots, keep, residuals)
}

/// Inlier correspondences of one pair in full-resolution pixels.
#[derive(Clone, Debug)]
pub struct PairPoints {
    pub a: usize,
    pub b: usize,
    pub pts: Vec<(f32, f32, f32, f32)>,
}

/// Robust global cost of a lens hypothesis: pairwise rotations from the inliers, averaged, then
/// the reprojection error of every inlier under the averaged rotations (Cauchy loss, pixels).
pub fn consistency_cost(
    n: usize,
    pairs: &[PairPoints],
    models: &[LensModel],
    lens_of: &[usize],
    anchor: usize,
) -> (f64, Vec<Option<Rotation3<f64>>>) {
    let edges: Vec<Edge> = pairs
        .iter()
        .filter_map(|p| {
            let (ma, mb) = (&models[lens_of[p.a]], &models[lens_of[p.b]]);
            let rays: Vec<(V3, V3)> = p
                .pts
                .iter()
                .map(|&(ax, ay, bx, by)| (ma.pixel_to_ray(ax as f64, ay as f64), mb.pixel_to_ray(bx as f64, by as f64)))
                .collect();
            let r = crate::matching::kabsch(rays.iter().copied())?;
            Some(Edge { a: p.a, b: p.b, r, w: (p.pts.len() as f64).sqrt() })
        })
        .collect();
    let (rots, _, _) = average_robust(n, &edges, anchor);
    let mut cost = 0.0;
    let mut count = 0usize;
    let c = 3.0; // Cauchy scale in pixels
    for p in pairs {
        let (Some(ra), Some(rb)) = (rots[p.a], rots[p.b]) else { continue };
        let (ma, mb) = (&models[lens_of[p.a]], &models[lens_of[p.b]]);
        for &(ax, ay, bx, by) in &p.pts {
            let da: V3 = ra * ma.pixel_to_ray(ax as f64, ay as f64);
            let db: V3 = rb * mb.pixel_to_ray(bx as f64, by as f64);
            let px = vec_angle(&da, &db) * ma.f;
            cost += (1.0 + (px / c).powi(2)).ln();
            count += 1;
        }
    }
    // Images left out of the main component count as maximal cost. The penalty is kept apart
    // from the mean so callers can judge how clear an optimum is from the mean alone.
    let placed = rots.iter().filter(|r| r.is_some()).count();
    let missing = n - placed;
    (cost / count.max(1) as f64 + missing as f64 * MISSING_COST, rots)
}

/// Penalty `consistency_cost` adds per photo outside the anchor's group.
pub const MISSING_COST: f64 = 10.0;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::rotation_from_ypr;

    #[test]
    fn averaging_recovers_ring_and_rejects_bad_edge() {
        let n = 12;
        let truth: Vec<Rotation3<f64>> =
            (0..n).map(|i| rotation_from_ypr(i as f64 * 30.0, if i % 2 == 0 { 5.0 } else { -5.0 }, 1.0)).collect();
        let mut edges = Vec::new();
        for i in 0..n {
            for d in [1usize, 2] {
                let j = (i + d) % n;
                // Small noise.
                let noise = Rotation3::from_euler_angles(0.001 * (i as f64).sin(), 0.001 * (j as f64).cos(), 0.0005);
                edges.push(Edge { a: i, b: j, r: truth[i].inverse() * truth[j] * noise, w: 10.0 });
            }
        }
        // A wrong edge (repeating texture).
        edges.push(Edge { a: 0, b: 6, r: Rotation3::from_euler_angles(0.0, 0.3, 0.0), w: 10.0 });
        let (rots, keep, _) = average_robust(n, &edges, 0);
        assert!(!keep[edges.len() - 1], "wrong edge should be dropped");
        for i in 0..n {
            let est = rots[i].unwrap();
            let t = truth[0].inverse() * truth[i];
            assert!(rot_angle(&(est.inverse() * t)) < 0.01, "image {i}: {}", rot_angle(&(est.inverse() * t)));
        }
    }
}

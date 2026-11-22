//! Seam placement: decides which photo supplies each panorama pixel.
//!
//! Starts from "use the photo whose centre is closest", then refines every overlapping pair with a
//! minimum cut that routes the seam through places where the two photos agree, so it avoids
//! cutting through moving objects and parallax errors. Runs on a reduced-size canvas.

use crate::warp::Layer;

pub const NONE: u16 = u16::MAX;

/// Initial labels: the valid layer with the highest centre weight at each pixel.
pub fn initial_labels(cw: usize, ch: usize, layers: &[Layer]) -> Vec<u16> {
    let mut labels = vec![NONE; cw * ch];
    let mut best = vec![0.0f32; cw * ch];
    for (li, layer) in layers.iter().enumerate() {
        let roi = layer.roi;
        for ly in 0..roi.h {
            let cy = roi.y0 + ly as i64;
            if cy < 0 || cy >= ch as i64 {
                continue;
            }
            for lx in 0..roi.w {
                let o = ly * roi.w + lx;
                if layer.alpha[o] == 0 {
                    continue;
                }
                let cx = (roi.x0 + lx as i64).rem_euclid(cw as i64) as usize;
                let c = cy as usize * cw + cx;
                let w = layer.weight[o];
                if w > best[c] {
                    best[c] = w;
                    labels[c] = li as u16;
                }
            }
        }
    }
    labels
}

/// Dinic max-flow on a 4-connected grid graph with terminal capacities folded into `excess`
/// (positive = connected to the source, negative = to the sink).
struct GridFlow {
    /// Neighbour node per direction (left, right, up, down) or `u32::MAX`.
    nbr: Vec<u32>,
    cap: Vec<i32>,
    excess: Vec<i32>,
}

const INF: i32 = 1 << 28;

impl GridFlow {
    fn run(&mut self) -> Vec<bool> {
        let n = self.excess.len();
        let mut level = vec![-1i32; n];
        let mut it = vec![0u8; n];
        let mut queue: Vec<u32> = Vec::with_capacity(n);
        let mut stack: Vec<(u32, u8)> = Vec::new();
        loop {
            // BFS from all source-connected nodes.
            level.iter_mut().for_each(|l| *l = -1);
            queue.clear();
            for v in 0..n {
                if self.excess[v] > 0 {
                    level[v] = 0;
                    queue.push(v as u32);
                }
            }
            let mut reached_sink = false;
            let mut head = 0;
            while head < queue.len() {
                let v = queue[head] as usize;
                head += 1;
                if self.excess[v] < 0 {
                    reached_sink = true;
                }
                for d in 0..4 {
                    let u = self.nbr[v * 4 + d];
                    if u != u32::MAX && self.cap[v * 4 + d] > 0 && level[u as usize] < 0 {
                        level[u as usize] = level[v] + 1;
                        queue.push(u);
                    }
                }
            }
            if !reached_sink {
                // Source side of the minimum cut = everything still reachable.
                return level.iter().map(|&l| l >= 0).collect();
            }
            it.iter_mut().for_each(|i| *i = 0);
            // Blocking flow by depth-first search with current-arc pointers.
            for s in 0..n {
                if level[s] != 0 || self.excess[s] <= 0 {
                    continue;
                }
                stack.clear();
                let mut v = s;
                loop {
                    if self.excess[v] < 0 {
                        // Reached a sink-connected node: push as much as the path allows.
                        let mut f = self.excess[s].min(-self.excess[v]);
                        for &(p, d) in &stack {
                            f = f.min(self.cap[p as usize * 4 + d as usize]);
                        }
                        let mut first_saturated = None;
                        for (k, &(p, d)) in stack.iter().enumerate() {
                            let (p, d) = (p as usize, d as usize);
                            self.cap[p * 4 + d] -= f;
                            let u = self.nbr[p * 4 + d] as usize;
                            self.cap[u * 4 + (d ^ 1)] += f;
                            if self.cap[p * 4 + d] == 0 && first_saturated.is_none() {
                                first_saturated = Some(k);
                            }
                        }
                        self.excess[s] -= f;
                        self.excess[v] += f;
                        if self.excess[s] == 0 {
                            break;
                        }
                        if let Some(k) = first_saturated {
                            // Resume from the tail of the first saturated arc.
                            v = stack[k].0 as usize;
                            stack.truncate(k);
                        }
                        continue;
                    }
                    let mut advanced = false;
                    while it[v] < 4 {
                        let d = it[v] as usize;
                        let u = self.nbr[v * 4 + d];
                        if u != u32::MAX && self.cap[v * 4 + d] > 0 && level[u as usize] == level[v] + 1 {
                            stack.push((v as u32, d as u8));
                            v = u as usize;
                            advanced = true;
                            break;
                        }
                        it[v] += 1;
                    }
                    if advanced {
                        continue;
                    }
                    // Dead end: retreat.
                    level[v] = -2;
                    match stack.pop() {
                        Some((p, _)) => {
                            v = p as usize;
                            it[v] += 1;
                        }
                        None => break,
                    }
                }
            }
        }
    }
}

/// Refines `labels` with pairwise minimum cuts. `wraps` makes the canvas horizontally periodic.
/// Returns the number of pairs processed.
pub fn refine_seams(labels: &mut [u16], cw: usize, ch: usize, layers: &[Layer], wraps: bool, passes: usize, band: usize) -> usize {
    let n = layers.len();
    let mut node_of = vec![u32::MAX; cw * ch];
    let mut pairs_done = 0;
    for _ in 0..passes {
        let mut changed = 0usize;
        for i in 0..n {
            for j in i + 1..n {
                // Quick reject on vertical extent.
                let (ri, rj) = (layers[i].roi, layers[j].roi);
                if ri.y0 + ri.h as i64 <= rj.y0 || rj.y0 + rj.h as i64 <= ri.y0 {
                    continue;
                }
                changed += cut_pair(labels, cw, ch, layers, i, j, wraps, band, &mut node_of);
                pairs_done += 1;
            }
        }
        if changed == 0 {
            break;
        }
    }
    pairs_done
}

#[allow(clippy::too_many_arguments)]
fn cut_pair(
    labels: &mut [u16],
    cw: usize,
    ch: usize,
    layers: &[Layer],
    i: usize,
    j: usize,
    wraps: bool,
    band: usize,
    node_of: &mut [u32],
) -> usize {
    let (la, lb) = (&layers[i], &layers[j]);
    let (li, lj) = (i as u16, j as u16);
    // Region: pixels covered by both photos and currently assigned to one of them.
    let mut nodes: Vec<u32> = Vec::new(); // canvas index per node
    let mut diff: Vec<i32> = Vec::new();
    let mut wa: Vec<f32> = Vec::new();
    let mut wb: Vec<f32> = Vec::new();
    let roi = la.roi;
    for ly in 0..roi.h {
        let cy = roi.y0 + ly as i64;
        if cy < 0 || cy >= ch as i64 {
            continue;
        }
        for lx in 0..roi.w {
            let oa = ly * roi.w + lx;
            if la.alpha[oa] == 0 {
                continue;
            }
            let cx = (roi.x0 + lx as i64).rem_euclid(cw as i64) as usize;
            let c = cy as usize * cw + cx;
            let l = labels[c];
            if l != li && l != lj {
                continue;
            }
            let Some(ob) = lb.index(cx, cy as usize, cw) else { continue };
            if lb.alpha[ob] == 0 {
                continue;
            }
            let (pa, pb) = (&la.rgb[oa * 3..oa * 3 + 3], &lb.rgb[ob * 3..ob * 3 + 3]);
            let d = (pa[0] - pb[0]).abs() + (pa[1] - pb[1]).abs() + (pa[2] - pb[2]).abs();
            node_of[c] = nodes.len() as u32;
            nodes.push(c as u32);
            diff.push(d as i32);
            wa.push(la.weight[oa]);
            wb.push(lb.weight[ob]);
        }
    }
    if nodes.len() < 16 {
        for &c in &nodes {
            node_of[c as usize] = u32::MAX;
        }
        return 0;
    }
    // Only a band around the current seam takes part in the cut: far from the seam the choice is
    // already clear, and a narrow problem keeps the minimum cut fast. Repeated passes let the
    // seam travel further if it needs to.
    let band = band.clamp(2, u16::MAX as usize - 1) as u16;
    let neighbours = |c: usize| {
        let (x, y) = (c % cw, c / cw);
        [
            if x > 0 {
                Some(c - 1)
            } else if wraps {
                Some(c + cw - 1)
            } else {
                None
            },
            if x + 1 < cw {
                Some(c + 1)
            } else if wraps {
                Some(c + 1 - cw)
            } else {
                None
            },
            if y > 0 { Some(c - cw) } else { None },
            if y + 1 < ch { Some(c + cw) } else { None },
        ]
    };
    let mut dist = vec![u16::MAX; nodes.len()];
    let mut frontier: Vec<u32> = Vec::new();
    for (v, &c) in nodes.iter().enumerate() {
        let l = labels[c as usize];
        let on_seam = neighbours(c as usize).iter().flatten().any(|&q| node_of[q] != u32::MAX && labels[q] != l);
        if on_seam {
            dist[v] = 0;
            frontier.push(v as u32);
        }
    }
    if frontier.is_empty() {
        for &c in &nodes {
            node_of[c as usize] = u32::MAX;
        }
        return 0;
    }
    let mut d = 0u16;
    while !frontier.is_empty() && d < band {
        d += 1;
        let mut next = Vec::new();
        for &v in &frontier {
            for q in neighbours(nodes[v as usize] as usize).into_iter().flatten() {
                let u = node_of[q];
                if u != u32::MAX && dist[u as usize] == u16::MAX {
                    dist[u as usize] = d;
                    next.push(u);
                }
            }
        }
        frontier = next;
    }
    // Compact to the band.
    {
        let mut keep = 0usize;
        for v in 0..nodes.len() {
            let c = nodes[v] as usize;
            if dist[v] != u16::MAX {
                node_of[c] = keep as u32;
                nodes[keep] = nodes[v];
                diff[keep] = diff[v];
                wa[keep] = wa[v];
                wb[keep] = wb[v];
                keep += 1;
            } else {
                node_of[c] = u32::MAX;
            }
        }
        nodes.truncate(keep);
        diff.truncate(keep);
        wa.truncate(keep);
        wb.truncate(keep);
    }
    let nn = nodes.len();
    let mut g = GridFlow { nbr: vec![u32::MAX; nn * 4], cap: vec![0; nn * 4], excess: vec![0; nn] };
    let (mut has_src, mut has_sink) = (false, false);
    for v in 0..nn {
        let c = nodes[v] as usize;
        let neigh = neighbours(c);
        for (d, q) in neigh.iter().enumerate() {
            let Some(q) = *q else { continue };
            let u = node_of[q];
            if u != u32::MAX {
                g.nbr[v * 4 + d] = u;
                // Cutting between two pixels costs how much the photos disagree there.
                g.cap[v * 4 + d] = diff[v] + diff[u as usize] + 2;
            } else if labels[q] == li {
                g.excess[v] = INF;
                has_src = true;
            } else if labels[q] == lj && g.excess[v] == 0 {
                g.excess[v] = -INF;
                has_sink = true;
            }
        }
    }
    // A photo with no exclusive territory next to the overlap keeps its core so it is not erased.
    if !has_src || !has_sink {
        let seed = |g: &mut GridFlow, w: &[f32], label: u16, sign: i32| {
            let max = nodes.iter().zip(w).filter(|(c, _)| labels[**c as usize] == label).map(|(_, w)| *w).fold(0.0f32, f32::max);
            if max <= 0.0 {
                return false;
            }
            let mut any = false;
            for v in 0..nn {
                if labels[nodes[v] as usize] == label && w[v] >= 0.6 * max && g.excess[v] == 0 {
                    g.excess[v] = sign * INF;
                    any = true;
                }
            }
            any
        };
        if !has_src {
            has_src = seed(&mut g, &wa, li, 1);
        }
        if !has_sink {
            has_sink = seed(&mut g, &wb, lj, -1);
        }
    }
    let mut changed = 0;
    if has_src && has_sink {
        let source_side = g.run();
        for v in 0..nn {
            let c = nodes[v] as usize;
            let new = if source_side[v] { li } else { lj };
            if labels[c] != new {
                labels[c] = new;
                changed += 1;
            }
        }
    }
    for &c in &nodes {
        node_of[c as usize] = u32::MAX;
    }
    changed
}

/// Carries labels from a coarser canvas (`factor` times smaller) to a finer one. A coarse label is
/// kept only where that layer really covers the fine pixel; elsewhere the fine initial label stays.
pub fn upsample_labels(coarse: &[u16], ccw: usize, cch: usize, fine: &mut [u16], cw: usize, ch: usize, layers: &[Layer]) {
    for y in 0..ch {
        let cy = (y * cch / ch).min(cch - 1);
        for x in 0..cw {
            let cx = (x * ccw / cw).min(ccw - 1);
            let l = coarse[cy * ccw + cx];
            if l == NONE {
                continue;
            }
            if let Some(o) = layers[l as usize].index(x, y, cw) {
                if layers[l as usize].alpha[o] != 0 {
                    fine[y * cw + x] = l;
                }
            }
        }
    }
}

/// Grows labels into unlabelled pixels by up to `radius` pixels (nearest label first), so that
/// upsampled masks do not fade out before the photo's true edge.
pub fn dilate_labels(labels: &mut [u16], cw: usize, ch: usize, wraps: bool, radius: usize) {
    let mut frontier: Vec<u32> = (0..cw * ch).filter(|&c| labels[c] != NONE).map(|c| c as u32).collect();
    for _ in 0..radius {
        let mut next = Vec::new();
        for &c in &frontier {
            let c = c as usize;
            let (x, y) = (c % cw, c / cw);
            let l = labels[c];
            let mut visit = |q: usize| {
                if labels[q] == NONE {
                    labels[q] = l;
                    next.push(q as u32);
                }
            };
            if x > 0 {
                visit(c - 1)
            } else if wraps {
                visit(c + cw - 1)
            }
            if x + 1 < cw {
                visit(c + 1)
            } else if wraps {
                visit(c + 1 - cw)
            }
            if y > 0 {
                visit(c - cw)
            }
            if y + 1 < ch {
                visit(c + cw)
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::warp::Roi;

    fn layer(x0: i64, w: usize, h: usize, f: impl Fn(usize, usize) -> f32) -> Layer {
        let mut rgb = vec![0.0; w * h * 3];
        let mut weight = vec![0.0; w * h];
        for y in 0..h {
            for x in 0..w {
                let v = f(x0 as usize + x, y);
                rgb[(y * w + x) * 3..(y * w + x) * 3 + 3].fill(v);
                let edge = x.min(w - 1 - x) as f32 / (w as f32 * 0.5);
                weight[y * w + x] = edge.max(1e-3);
            }
        }
        Layer { roi: Roi { x0, y0: 0, w, h }, rgb, alpha: vec![255; w * h], weight }
    }

    #[test]
    fn max_flow_simple_chain() {
        // source — a — b — sink with a weak middle link: cut separates a from b.
        let mut g = GridFlow { nbr: vec![u32::MAX; 8], cap: vec![0; 8], excess: vec![INF, -INF] };
        g.nbr[1] = 1; // a → right → b
        g.cap[1] = 5;
        g.nbr[4] = 0; // b → left → a
        g.cap[4] = 5;
        let side = g.run();
        assert_eq!(side, vec![true, false]);
    }

    #[test]
    fn seam_follows_the_agreement_valley() {
        // Two layers overlap in x ∈ [40, 80). They agree only along a slanted line; elsewhere they differ.
        let (cw, ch) = (120, 60);
        let valley = |y: usize| 50 + y / 3;
        let a = layer(0, 80, ch, |_, _| 100.0);
        let b = layer(40, 80, ch, |x, y| if x == valley(y) || x + 1 == valley(y) { 100.0 } else { 160.0 });
        let layers = vec![a, b];
        let mut labels = initial_labels(cw, ch, &layers);
        refine_seams(&mut labels, cw, ch, &layers, false, 2, 1000);
        for y in 2..ch - 2 {
            let row = &labels[y * cw..(y + 1) * cw];
            let switch = row.iter().position(|&l| l == 1).unwrap();
            assert!((switch as i64 - valley(y) as i64).abs() <= 2, "row {y}: seam at {switch}, valley at {}", valley(y));
            assert!(row[switch..].iter().all(|&l| l == 1));
        }
    }

    #[test]
    fn dilation_fills_gaps_near_labels() {
        let mut labels = vec![NONE; 10];
        labels[4] = 3;
        dilate_labels(&mut labels, 10, 1, true, 2);
        assert_eq!(&labels[2..7], &[3, 3, 3, 3, 3]);
        assert_eq!(labels[0], NONE);
    }
}

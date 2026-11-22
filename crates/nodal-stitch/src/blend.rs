//! Multi-band (Laplacian pyramid) blending.
//!
//! Each warped photo is decomposed into frequency bands; every band is mixed across the seam over
//! a distance matched to its wavelength. Low frequencies (exposure, sky gradients) blend over
//! hundreds of pixels while fine detail switches within a few, so seams vanish without ghosting.
//!
//! Layers are fed one at a time, so memory stays at one output pyramid plus the current layer.
//! Photos are extrapolated smoothly beyond their own borders (pull–push) so coverage edges do
//! not bleed black into the bands.

use rayon::prelude::*;

/// A float image with `c` interleaved channels.
#[derive(Clone)]
pub struct Plane {
    pub w: usize,
    pub h: usize,
    pub c: usize,
    pub data: Vec<f32>,
}

impl Plane {
    pub fn new(w: usize, h: usize, c: usize) -> Plane {
        Plane { w, h, c, data: vec![0.0; w * h * c] }
    }
}

/// Burt–Adelson reduce: 5-tap binomial filter, then keep even samples. Borders clamp.
pub fn reduce(src: &Plane) -> Plane {
    let (w, h, c) = (src.w, src.h, src.c);
    let (nw, nh) = (w.div_ceil(2), h.div_ceil(2));
    // Horizontal.
    let mut tmp = Plane::new(nw, h, c);
    tmp.data.par_chunks_mut(nw * c).zip(src.data.par_chunks(w * c)).for_each(|(out, row)| {
        for x in 0..nw {
            let xc = 2 * x;
            let i = |d: isize| ((xc as isize + d).clamp(0, w as isize - 1) as usize) * c;
            let (a, b, m, d, e) = (i(-2), i(-1), i(0), i(1), i(2));
            for k in 0..c {
                out[x * c + k] = (row[a + k] + row[e + k] + 4.0 * (row[b + k] + row[d + k]) + 6.0 * row[m + k]) * (1.0 / 16.0);
            }
        }
    });
    // Vertical.
    let mut out = Plane::new(nw, nh, c);
    let stride = nw * c;
    out.data.par_chunks_mut(stride).enumerate().for_each(|(y, o)| {
        let r = |d: isize| {
            let yy = (2 * y as isize + d).clamp(0, h as isize - 1) as usize;
            &tmp.data[yy * stride..(yy + 1) * stride]
        };
        let (a, b, m, d, e) = (r(-2), r(-1), r(0), r(1), r(2));
        for i in 0..stride {
            o[i] = (a[i] + e[i] + 4.0 * (b[i] + d[i]) + 6.0 * m[i]) * (1.0 / 16.0);
        }
    });
    out
}

/// Burt–Adelson expand to `(w, h)` (the size the source was reduced from).
pub fn expand(src: &Plane, w: usize, h: usize) -> Plane {
    let (sw, sh, c) = (src.w, src.h, src.c);
    // Vertical.
    let mut tmp = Plane::new(sw, h, c);
    let stride = sw * c;
    tmp.data.par_chunks_mut(stride).enumerate().for_each(|(y, o)| {
        let j = y / 2;
        let r = |d: isize| {
            let yy = (j as isize + d).clamp(0, sh as isize - 1) as usize;
            &src.data[yy * stride..(yy + 1) * stride]
        };
        if y % 2 == 0 {
            let (a, m, b) = (r(-1), r(0), r(1));
            for i in 0..stride {
                o[i] = (a[i] + b[i] + 6.0 * m[i]) * 0.125;
            }
        } else {
            let (m, b) = (r(0), r(1));
            for i in 0..stride {
                o[i] = (m[i] + b[i]) * 0.5;
            }
        }
    });
    // Horizontal.
    let mut out = Plane::new(w, h, c);
    out.data.par_chunks_mut(w * c).zip(tmp.data.par_chunks(stride)).for_each(|(o, row)| {
        for x in 0..w {
            let j = x / 2;
            let i = |d: isize| ((j as isize + d).clamp(0, sw as isize - 1) as usize) * c;
            if x % 2 == 0 {
                let (a, m, b) = (i(-1), i(0), i(1));
                for k in 0..c {
                    o[x * c + k] = (row[a + k] + row[b + k] + 6.0 * row[m + k]) * 0.125;
                }
            } else {
                let (m, b) = (i(0), i(1));
                for k in 0..c {
                    o[x * c + k] = (row[m + k] + row[b + k]) * 0.5;
                }
            }
        }
    });
    out
}

/// Builds a colour pyramid whose levels are extrapolated beyond the valid region (pull–push).
/// `rgb` must be zero where `alpha` is zero. Returns the filled Gaussian levels, finest first.
pub fn extrapolated_pyramid(rgb: Plane, alpha: Plane, levels: usize) -> Vec<Plane> {
    let mut g = vec![rgb];
    let mut a = vec![alpha];
    // Pull: alpha-weighted averages.
    for l in 0..levels {
        let mut pre = g[l].clone();
        if l > 0 {
            // Levels above 0 hold normalised colour; premultiply again by their coverage.
            pre.data.par_chunks_mut(3).zip(a[l].data.par_iter()).for_each(|(p, &al)| {
                p[0] *= al;
                p[1] *= al;
                p[2] *= al;
            });
        }
        let mut next = reduce(&pre);
        let next_a = reduce(&a[l]);
        next.data.par_chunks_mut(3).zip(next_a.data.par_iter()).for_each(|(p, &al)| {
            if al > 1e-6 {
                let inv = 1.0 / al;
                p[0] *= inv;
                p[1] *= inv;
                p[2] *= inv;
            } else {
                p.fill(0.0);
            }
        });
        g.push(next);
        a.push(next_a);
    }
    // Push: fill uncovered pixels of each level from the coarser one.
    for l in (0..levels).rev() {
        let e = expand(&g[l + 1], g[l].w, g[l].h);
        let al = &a[l];
        g[l].data.par_chunks_mut(3).zip(e.data.par_chunks(3)).zip(al.data.par_iter()).for_each(|((p, q), &w)| {
            let w = w.min(1.0);
            p[0] = p[0] * w + q[0] * (1.0 - w);
            p[1] = p[1] * w + q[1] * (1.0 - w);
            p[2] = p[2] * w + q[2] * (1.0 - w);
        });
    }
    g
}

/// Accumulates layers into an output pyramid and collapses it.
pub struct Blender {
    pub w: usize,
    pub h: usize,
    pub levels: usize,
    acc: Vec<Plane>,
    wsum: Vec<Plane>,
}

impl Blender {
    /// `levels` is the number of reductions (0 = plain masked compositing).
    pub fn new(w: usize, h: usize, levels: usize) -> Blender {
        let mut acc = Vec::new();
        let mut wsum = Vec::new();
        let (mut lw, mut lh) = (w, h);
        for _ in 0..=levels {
            acc.push(Plane::new(lw, lh, 3));
            wsum.push(Plane::new(lw, lh, 1));
            lw = lw.div_ceil(2);
            lh = lh.div_ceil(2);
        }
        Blender { w, h, levels, acc, wsum }
    }

    /// Alignment required of layer origins and sizes.
    pub fn align(&self) -> usize {
        1 << self.levels
    }

    /// Adds a layer. `(x0, y0)` and the size must be multiples of [`Blender::align`]; the layer may
    /// extend past the canvas (the excess is ignored). `alpha` is coverage in [0, 1] (rgb must be 0
    /// where it is 0) and `mask ≤ alpha` is this layer's share of the final image.
    pub fn feed(&mut self, x0: i64, y0: i64, w: usize, h: usize, rgb: Vec<f32>, alpha: Vec<f32>, mask: Vec<f32>) {
        let levels = self.levels;
        debug_assert!(w.is_multiple_of(self.align()) && h.is_multiple_of(self.align()));
        let g = extrapolated_pyramid(Plane { w, h, c: 3, data: rgb }, Plane { w, h, c: 1, data: alpha }, levels);
        let mut m = vec![Plane { w, h, c: 1, data: mask }];
        for l in 0..levels {
            let next = reduce(&m[l]);
            m.push(next);
        }
        for l in 0..=levels {
            // Band-pass content of this level.
            let lap = if l < levels {
                let e = expand(&g[l + 1], g[l].w, g[l].h);
                let mut d = g[l].clone();
                d.data.par_iter_mut().zip(e.data.par_iter()).for_each(|(a, b)| *a -= b);
                d
            } else {
                g[l].clone()
            };
            let (lx0, ly0) = (x0 >> l, y0 >> l);
            let (acc, wsum) = (&mut self.acc[l], &mut self.wsum[l]);
            let (aw, ah) = (acc.w as i64, acc.h as i64);
            let (lw, lh) = (lap.w, lap.h);
            let mask = &m[l];
            // Rows of the layer that fall inside the canvas.
            let y_start = (-ly0).max(0) as usize;
            let y_end = ((ah - ly0).min(lh as i64)).max(0) as usize;
            let x_start = (-lx0).max(0) as usize;
            let x_end = ((aw - lx0).min(lw as i64)).max(0) as usize;
            if y_start >= y_end || x_start >= x_end {
                continue;
            }
            let dst_x = (lx0 + x_start as i64) as usize;
            let dst_y = (ly0 + y_start as i64) as usize;
            let n = x_end - x_start;
            acc.data.par_chunks_mut(acc.w * 3).zip(wsum.data.par_chunks_mut(acc.w)).skip(dst_y).take(y_end - y_start).enumerate().for_each(
                |(i, (arow, wrow))| {
                    let ly = y_start + i;
                    let lrow = &lap.data[(ly * lw + x_start) * 3..(ly * lw + x_end) * 3];
                    let mrow = &mask.data[ly * lw + x_start..ly * lw + x_end];
                    let arow = &mut arow[dst_x * 3..(dst_x + n) * 3];
                    let wrow = &mut wrow[dst_x..dst_x + n];
                    for k in 0..n {
                        let mv = mrow[k];
                        if mv > 0.0 {
                            arow[k * 3] += lrow[k * 3] * mv;
                            arow[k * 3 + 1] += lrow[k * 3 + 1] * mv;
                            arow[k * 3 + 2] += lrow[k * 3 + 2] * mv;
                            wrow[k] += mv;
                        }
                    }
                },
            );
        }
    }

    /// Collapses the pyramid. Returns RGB (3 floats per pixel, 8-bit units) and the level-0 weight
    /// sum (> 0 where any layer contributed).
    pub fn finish(mut self) -> (Vec<f32>, Vec<f32>) {
        const EPS: f32 = 1e-7;
        let normalise = |acc: &mut Plane, wsum: &Plane| {
            acc.data.par_chunks_mut(3).zip(wsum.data.par_iter()).for_each(|(p, &w)| {
                if w > EPS {
                    let inv = 1.0 / w;
                    p[0] *= inv;
                    p[1] *= inv;
                    p[2] *= inv;
                } else {
                    p.fill(0.0);
                }
            });
        };
        let top = self.levels;
        let mut cur = self.acc.pop().unwrap();
        normalise(&mut cur, &self.wsum[top]);
        for l in (0..top).rev() {
            let mut band = self.acc.pop().unwrap();
            normalise(&mut band, &self.wsum[l]);
            let e = expand(&cur, band.w, band.h);
            band.data.par_iter_mut().zip(e.data.par_iter()).for_each(|(a, b)| *a += b);
            cur = band;
        }
        (cur.data, std::mem::take(&mut self.wsum[0].data))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reduce_expand_preserve_constants() {
        let mut p = Plane::new(37, 21, 3);
        p.data.fill(5.0);
        let r = reduce(&p);
        assert_eq!((r.w, r.h), (19, 11));
        assert!(r.data.iter().all(|v| (v - 5.0).abs() < 1e-5));
        let e = expand(&r, 37, 21);
        assert!(e.data.iter().all(|v| (v - 5.0).abs() < 1e-5));
    }

    #[test]
    fn single_layer_reconstructs_exactly() {
        let (w, h) = (64, 32);
        let rgb: Vec<f32> = (0..w * h * 3).map(|i| ((i * 37) % 251) as f32).collect();
        let mut b = Blender::new(w, h, 4);
        b.feed(0, 0, w, h, rgb.clone(), vec![1.0; w * h], vec![1.0; w * h]);
        let (out, cov) = b.finish();
        assert!(cov.iter().all(|&c| c > 0.99));
        let max_err = out.iter().zip(&rgb).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
        assert!(max_err < 1e-2, "{max_err}");
    }

    #[test]
    fn two_layers_blend_smoothly_and_keep_coverage_edges_clean() {
        // Left layer is brighter than the right one; both only cover part of the canvas.
        let (w, h) = (128, 32);
        let mut blender = Blender::new(w, h, 4);
        let mk = |val: f32, x_from: usize, x_to: usize, m_from: usize, m_to: usize| {
            let mut rgb = vec![0.0; w * h * 3];
            let mut alpha = vec![0.0; w * h];
            let mut mask = vec![0.0; w * h];
            for y in 0..h {
                for x in x_from..x_to {
                    rgb[(y * w + x) * 3..(y * w + x) * 3 + 3].fill(val);
                    alpha[y * w + x] = 1.0;
                    if x >= m_from && x < m_to {
                        mask[y * w + x] = 1.0;
                    }
                }
            }
            (rgb, alpha, mask)
        };
        let (r, a, m) = mk(100.0, 8, 80, 8, 64);
        blender.feed(0, 0, w, h, r, a, m);
        let (r, a, m) = mk(140.0, 48, 120, 64, 120);
        blender.feed(0, 0, w, h, r, a, m);
        let (out, cov) = blender.finish();
        let px = |x: usize| out[(16 * w + x) * 3];
        // Coverage edges keep their own colour (no dark halo from the empty area).
        assert!((px(9) - 100.0).abs() < 3.0, "{}", px(9));
        assert!((px(118) - 140.0).abs() < 3.0, "{}", px(118));
        // The transition is monotonic and gradual around the seam.
        assert!(px(63) > 105.0 && px(64) < 135.0);
        for x in 9..118 {
            assert!(px(x + 1) >= px(x) - 0.01);
        }
        assert!(cov[16 * w + 4] == 0.0 && cov[16 * w + 60] > 0.5);
    }
}

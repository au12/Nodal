//! Cube face rendering and the face image pyramid.

use crate::sampler::Equirect;
use nodal_core::cube::Face;
use rayon::prelude::*;
use std::sync::OnceLock;

/// An RGB8 face image with a margin ring of `margin` pixels around a `size`² face.
pub struct FaceImage {
    pub size: u32,
    pub margin: u32,
    pub data: Vec<u8>,
}

impl FaceImage {
    #[inline]
    pub fn stride(&self) -> usize {
        (self.size + 2 * self.margin) as usize
    }

    /// Copies a `slot`² square whose top-left corner is at face pixel `(x, y)`
    /// (face pixel coordinates; negative values reach into the margin).
    pub fn crop(&self, x: i64, y: i64, slot: u32) -> Vec<u8> {
        let stride = self.stride();
        let mut out = Vec::with_capacity((slot * slot * 3) as usize);
        for row in 0..slot as i64 {
            let sy = (y + row + self.margin as i64) as usize;
            let sx = (x + self.margin as i64) as usize;
            let start = (sy * stride + sx) * 3;
            out.extend_from_slice(&self.data[start..start + slot as usize * 3]);
        }
        out
    }

    /// Halves the image with a 2×2 box filter in linear light.
    pub fn downsample(&self) -> FaceImage {
        let (to_lin, to_srgb) = luts();
        let src_stride = self.stride();
        let new = FaceImage { size: self.size / 2, margin: self.margin / 2, data: Vec::new() };
        let dst_stride = new.stride();
        let mut data = vec![0u8; dst_stride * dst_stride * 3];
        data.par_chunks_mut(dst_stride * 3).enumerate().for_each(|(y, row)| {
            let r0 = &self.data[(2 * y) * src_stride * 3..(2 * y + 1) * src_stride * 3];
            let r1 = &self.data[(2 * y + 1) * src_stride * 3..(2 * y + 2) * src_stride * 3];
            for x in 0..dst_stride {
                for c in 0..3 {
                    let i = 2 * x * 3 + c;
                    let s = to_lin[r0[i] as usize] + to_lin[r0[i + 3] as usize] + to_lin[r1[i] as usize] + to_lin[r1[i + 3] as usize];
                    row[x * 3 + c] = to_srgb[((s * 0.25) * (LUT_N - 1) as f32 + 0.5) as usize];
                }
            }
        });
        FaceImage { data, ..new }
    }
}

const LUT_N: usize = 1 << 14;

fn luts() -> &'static ([f32; 256], Vec<u8>) {
    static L: OnceLock<([f32; 256], Vec<u8>)> = OnceLock::new();
    L.get_or_init(|| {
        let mut to_lin = [0.0f32; 256];
        for (i, v) in to_lin.iter_mut().enumerate() {
            *v = nodal_core::math::srgb_to_linear(i as f32 / 255.0);
        }
        let to_srgb = (0..LUT_N)
            .map(|i| (nodal_core::math::linear_to_srgb(i as f32 / (LUT_N - 1) as f32) * 255.0 + 0.5).clamp(0.0, 255.0) as u8)
            .collect();
        (to_lin, to_srgb)
    })
}

/// Renders a cube face of `size`² pixels plus `margin` pixels on every side. The margin
/// continues the face plane, so it contains the true neighbouring content.
pub fn render_face(eq: &Equirect, face: Face, size: u32, margin: u32) -> FaceImage {
    let n = (size + 2 * margin) as usize;
    let mut data = vec![0u8; n * n * 3];
    let (c, r, dn) = face.basis();
    let inv = 1.0 / size as f32;
    let step = 2.0 * inv; // plane units per pixel
    data.par_chunks_mut(n * 3).enumerate().for_each(|(j, row)| {
        let y = 2.0 * ((j as f32 - margin as f32 + 0.5) * inv) - 1.0;
        for i in 0..n {
            let x = 2.0 * ((i as f32 - margin as f32 + 0.5) * inv) - 1.0;
            let p = c + r * x + dn * y;
            let r2 = x * x + y * y;
            // Angular size of a pixel at this point of the face plane (geometric mean of the
            // radial (1/(1+r²)) and tangential (1/√(1+r²)) foreshortening).
            let s = (1.0 + r2).sqrt();
            let pixel_angle = step / (s * s.sqrt());
            let px = eq.sample_adaptive(p, pixel_angle);
            row[i * 3..i * 3 + 3].copy_from_slice(&px);
        }
    });
    FaceImage { size, margin, data }
}

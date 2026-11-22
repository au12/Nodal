//! Pole-patch check: cuts caps out of a complete panorama, fills them, and saves the result plus
//! straight-down / straight-up views for inspection.
//!   eval_fill pano.jpg out_prefix [nadir_deg] [zenith_deg] [width]
use nodal_stitch::fill::{fill_poles, FillOptions};
use nodal_stitch::render::Panorama;
use std::time::Instant;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let nadir: f32 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(28.0);
    let zenith: f32 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(22.0);
    let mut img = image::open(&args[1])?.into_rgb8();
    if let Some(w) = args.get(5).and_then(|s| s.parse::<u32>().ok()) {
        img = image::imageops::resize(&img, w, w / 2, image::imageops::FilterType::Lanczos3);
    }
    let (w, h) = (img.width() as usize, img.height() as usize);
    let mut alpha = vec![255u8; w * h];
    let mut rgb = img.into_raw();
    for y in 0..h {
        let colat = (y as f32 + 0.5) / h as f32 * 180.0;
        // Slightly irregular edge, like real coverage.
        for x in 0..w {
            let wobble = 2.0 * ((x as f32 / w as f32) * 6.0 * std::f32::consts::TAU).sin();
            if colat < zenith + wobble || 180.0 - colat < nadir + wobble {
                alpha[y * w + x] = 0;
                rgb[(y * w + x) * 3..(y * w + x) * 3 + 3].fill(0);
            }
        }
    }
    let mut pano =
        Panorama { width: w, height: h, rgb, alpha, full_width: w, full_height: h, crop_x: 0, crop_y: 0, stats: Default::default() };
    let t = Instant::now();
    let report = fill_poles(&mut pano, &FillOptions::default(), &|_, _| {});
    println!("{report:?} in {:.2?}", t.elapsed());
    pano.to_image().save(format!("{}.jpg", args[2]))?;
    Ok(())
}

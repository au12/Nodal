//! Ground-truth alignment check: renders synthetic shots from an equirect and measures how well the
//! stitcher recovers their orientations and lens.
//!   cargo run --release -p nodal-stitch --example eval_align -- pano.jpg [fisheye|rect] [fov]

use nalgebra::Rotation3;
use nodal_stitch::features::{detect, DetectOptions};
use nodal_stitch::project::rotation_from_ypr;
use nodal_stitch::synth::{render_shot, ShotOptions};
use nodal_stitch::{align, Lens, LensKind};
use std::time::Instant;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let pano = image::open(&args[1])?.into_rgb8();
    let fisheye = args.get(2).is_some_and(|s| s == "fisheye");
    let (lens, poses): (Lens, Vec<(f64, f64, f64)>) = if fisheye {
        let mut l = Lens::new(LensKind::Fisheye, 150.0, 1500, 2250);
        l.b = -0.03;
        l.a = 0.01;
        (l, vec![(0.0, 2.0, 1.0), (61.0, -1.0, -2.0), (119.0, 3.0, 0.5), (182.0, 0.0, 1.5), (240.0, -2.0, -1.0), (301.0, 1.0, 0.0), (10.0, 88.0, 0.0), (30.0, -86.0, 5.0)])
    } else {
        let fov: f64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(65.0);
        let mut l = Lens::new(LensKind::Rectilinear, fov, 1800, 1200);
        l.b = -0.05;
        l.a = 0.015;
        l.d = 14.0;
        l.e = -9.0;
        let mut poses = Vec::new();
        for (pitch, n) in [(0.0, 9), (38.0, 7), (-38.0, 7), (72.0, 4), (-72.0, 4)] {
            for k in 0..n {
                poses.push((k as f64 * 360.0 / n as f64 + pitch * 0.3, pitch + (k % 3) as f64 - 1.0, (k as f64 * 1.7) % 4.0 - 2.0));
            }
        }
        (l, poses)
    };
    let t = Instant::now();
    let truth: Vec<Rotation3<f64>> = poses.iter().map(|&(y, p, r)| rotation_from_ypr(y, p, r)).collect();
    let shots: Vec<_> = truth
        .iter()
        .enumerate()
        .map(|(i, r)| render_shot(&pano, &lens, r, &ShotOptions { gain: 1.0 + 0.15 * ((i % 3) as f32 - 1.0), vignette: 0.25, noise: 2.0, seed: i as u64 }))
        .collect();
    println!("rendered {} shots in {:.2?}", shots.len(), t.elapsed());
    if let Some(dir) = args.get(4) {
        std::fs::create_dir_all(dir)?;
        for (i, s) in shots.iter().enumerate() {
            s.save(format!("{dir}/shot{i:02}.jpg"))?;
        }
    }

    let t = Instant::now();
    let feats: Vec<_> = shots.iter().map(|s| detect(s.as_raw(), s.width() as usize, s.height() as usize, &DetectOptions::default())).collect();
    println!("features: {:?} in {:.2?}", feats.iter().map(|f| f.len()).collect::<Vec<_>>(), t.elapsed());

    let meta: Vec<_> = shots.iter().enumerate().map(|(i, s)| (format!("shot{i}.jpg").into(), s.width(), s.height(), Default::default())).collect();
    let mut project = align::new_project(&meta);
    let t = Instant::now();
    let report = align::align(&mut project, &feats, &align::AlignOptions { level: false, ..Default::default() }, &|_, _| {})?;
    println!("aligned in {:.2?}: {} pairs, {} points, RMS {:.3} px, max {:.2} px", t.elapsed(), report.pairs.len(), report.control_points, report.rms, report.max);
    for l in &report.log {
        println!("  {l}");
    }
    let l = project.lenses[0];
    println!("lens: {:?} hfov {:.3} (truth {:.3})  a {:.4} b {:.4} c {:.4} d {:.1} e {:.1}  (truth a {} b {} d {} e {})", l.kind, l.hfov, lens.hfov, l.a, l.b, l.c, l.d, l.e, lens.a, lens.b, lens.d, lens.e);
    // Compare relative orientations with ground truth (global rotation is arbitrary).
    let g = truth[0] * project.images[0].rot().inverse();
    let mut worst = 0.0f64;
    let mut sum = 0.0;
    for (i, img) in project.images.iter().enumerate() {
        if !img.aligned {
            println!("  image {i} NOT aligned");
            continue;
        }
        let d = (g * img.rot()).inverse() * truth[i];
        let err = (d.matrix() - nalgebra::Matrix3::identity()).norm().to_degrees() / std::f64::consts::SQRT_2;
        if err > 0.05 { println!("  image {i}: {err:.4}°"); }
        worst = worst.max(err);
        sum += err;
    }
    println!("orientation error: mean {:.4}°, worst {:.4}°  (one source pixel ≈ {:.4}°)", sum / project.images.len() as f64, worst, lens.model().pixel_angle().to_degrees());
    Ok(())
}

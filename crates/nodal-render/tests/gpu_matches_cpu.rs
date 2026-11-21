//! End-to-end check: tiler → page table → shader must agree with the CPU reference
//! renderer (which samples the source equirect directly) for every projection.

use nodal_core::{Projection, View};
use nodal_render::headless::Headless;
use nodal_tiler::sampler::{Coverage, Equirect};
use std::time::Duration;

fn synthetic(w: u32, h: u32) -> image::RgbImage {
    image::RgbImage::from_fn(w, h, |x, y| {
        let u = x as f32 / w as f32;
        let v = y as f32 / h as f32;
        let checker = (((u * 24.0) as i32 + (v * 12.0) as i32) % 2) as f32;
        let r = 0.5 + 0.5 * (u * std::f32::consts::TAU * 3.0).sin();
        let g = v;
        let b = 0.25 + 0.5 * checker;
        image::Rgb([(r * 255.0) as u8, (g * 255.0) as u8, (b * 255.0) as u8])
    })
}

fn psnr(a: &image::RgbaImage, b: &image::RgbImage) -> f64 {
    let mut se = 0.0f64;
    let mut n = 0.0f64;
    for (pa, pb) in a.pixels().zip(b.pixels()) {
        for c in 0..3 {
            let d = pa[c] as f64 - pb[c] as f64;
            se += d * d;
            n += 1.0;
        }
    }
    10.0 * (255.0f64 * 255.0 / (se / n)).log10()
}

#[test]
fn gpu_render_matches_cpu_reference() {
    let mut hl = match Headless::new() {
        Ok(h) => h,
        Err(e) => {
            eprintln!("skipping GPU test: {e}");
            return;
        }
    };
    eprintln!("adapter: {}", hl.adapter);
    let dir = tempfile::tempdir().unwrap();
    let img = synthetic(4096, 2048);
    let path = dir.path().join("synthetic.png");
    img.save(&path).unwrap();
    let tour_dir = dir.path().join("tour");
    let tour = nodal_tiler::build_tour(&[path], &tour_dir, Some("t"), &Default::default(), &nodal_tiler::no_progress).unwrap();
    let scene = &tour.scenes[0];
    let eq = Equirect::new(img, Coverage::FULL);
    let out = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("gpu_vs_cpu");
    std::fs::create_dir_all(&out).unwrap();
    let views = [
        ("rectilinear", View { yaw: 20.0, pitch: 10.0, fov: 90.0, ..View::default() }),
        ("zoomed", View { yaw: -60.0, pitch: -5.0, fov: 25.0, ..View::default() }),
        ("seam_corner", View { yaw: 45.0, pitch: 35.0, fov: 70.0, ..View::default() }),
        ("zenith", View { yaw: 0.0, pitch: 80.0, fov: 100.0, ..View::default() }),
        ("fisheye", View { yaw: 100.0, pitch: 0.0, fov: 160.0, projection: Projection::FISHEYE, ..View::default() }),
        ("little_planet", View { yaw: 0.0, pitch: -90.0, fov: 280.0, projection: Projection::STEREOGRAPHIC, ..View::default() }),
        ("pannini", View { yaw: 180.0, pitch: 0.0, fov: 140.0, projection: Projection::PANNINI, ..View::default() }),
        (
            "architectural",
            View {
                yaw: 30.0,
                pitch: 25.0,
                fov: 90.0,
                projection: Projection { architectural: 1.0, ..Projection::RECTILINEAR },
                ..View::default()
            },
        ),
        ("equirect", View { yaw: 0.0, pitch: 0.0, fov: 360.0, projection: Projection::EQUIRECT, ..View::default() }),
    ];
    let mut worst = f64::MAX;
    for (name, view) in views {
        let gpu = hl.render(tour_dir.to_str().unwrap(), scene, [0, 0, 0], &view, 480, 300, Duration::from_secs(60)).unwrap();
        let cpu = nodal_tiler::render_view(&eq, &view, 480, 300, 3);
        gpu.save(out.join(format!("{name}_gpu.png"))).unwrap();
        cpu.save(out.join(format!("{name}_cpu.png"))).unwrap();
        let p = psnr(&gpu, &cpu);
        eprintln!("{name:>14}: PSNR {p:.1} dB");
        // The flat overview squeezes 360° into 480 px; there the comparison measures the
        // difference between two minification filters (3×3 supersampling vs trilinear tiles).
        let need = if name == "equirect" { 24.0 } else { 30.0 };
        assert!(p > need, "{name}: GPU and CPU renders diverge ({p:.1} dB < {need} dB)");
        worst = worst.min(p);
    }
    eprintln!("images in {}", out.display());
    eprintln!("worst PSNR {worst:.1} dB");
}

//! End-to-end checks against ground truth: render synthetic shots of a known panorama with a known
//! lens, stitch them, and compare with the original.

use nalgebra::Rotation3;
use nodal_stitch::features::{detect, DetectOptions};
use nodal_stitch::project::rotation_from_ypr;
use nodal_stitch::synth::{procedural_pano, render_shot, sample_bicubic, ShotOptions};
use nodal_stitch::{align, render, Lens, LensKind, Project, RenderOptions};
use std::sync::Arc;

struct Scene {
    pano: image::RgbImage,
    truth: Vec<Rotation3<f64>>,
    shots: Vec<Arc<image::RgbImage>>,
    project: Project,
}

fn build(lens: Lens, poses: &[(f64, f64, f64)], vary_exposure: bool, with_exif: bool) -> Scene {
    let pano = procedural_pano(2048, 11);
    let truth: Vec<_> = poses.iter().map(|&(y, p, r)| rotation_from_ypr(y, p, r)).collect();
    let shots: Vec<_> = truth
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let gain = if vary_exposure { 1.0 + 0.2 * ((i % 3) as f32 - 1.0) } else { 1.0 };
            Arc::new(render_shot(&pano, &lens, r, &ShotOptions { gain, vignette: 0.0, noise: 1.0, seed: i as u64 }))
        })
        .collect();
    let feats: Vec<_> =
        shots.iter().map(|s| detect(s.as_raw(), s.width() as usize, s.height() as usize, &DetectOptions::default())).collect();
    // Cameras normally record the focal length; partial panoramas cannot recover it from pixels alone.
    let exif = nodal_stitch::project::ExifInfo {
        focal_35mm: with_exif.then(|| Lens::new(lens.kind, lens.hfov, lens.width, lens.height).focal_35mm()),
        ..Default::default()
    };
    let meta: Vec<_> =
        shots.iter().enumerate().map(|(i, s)| (format!("shot{i}.jpg").into(), s.width(), s.height(), exif.clone())).collect();
    let mut project = align::new_project(&meta);
    let report = align::align(&mut project, &feats, &align::AlignOptions { level: false, ..Default::default() }, &|_, _| {}).unwrap();
    assert!(report.unaligned.is_empty(), "unaligned images: {:?}", report.unaligned);
    // Put the panorama in the ground-truth frame (the global rotation is otherwise arbitrary).
    let g = truth[0] * project.images[0].rot().inverse();
    project.rotate_world(&g);
    Scene { pano, truth, shots, project }
}

fn orientation_error_deg(scene: &Scene) -> f64 {
    scene
        .project
        .images
        .iter()
        .zip(&scene.truth)
        .map(|(img, t)| ((img.rot().inverse() * t).matrix() - nalgebra::Matrix3::identity()).norm().to_degrees() / std::f64::consts::SQRT_2)
        .fold(0.0, f64::max)
}

/// PSNR of the stitched panorama against the ground truth, over covered pixels away from the poles.
fn psnr(scene: &Scene, pano: &nodal_stitch::Panorama) -> (f64, f64) {
    let (gw, gh) = (scene.pano.width() as usize, scene.pano.height() as usize);
    let (mut se, mut n, mut covered) = (0.0f64, 0usize, 0usize);
    for y in 0..pano.height {
        for x in 0..pano.width {
            let o = y * pano.width + x;
            if pano.alpha[o] == 0 {
                continue;
            }
            covered += 1;
            let fy = (y + pano.crop_y) as f32 + 0.5;
            if (fy / pano.full_height as f32 - 0.5).abs() > 0.42 {
                continue;
            }
            let gx = ((x + pano.crop_x) as f32 + 0.5) * gw as f32 / pano.full_width as f32;
            let gy = fy * gh as f32 / pano.full_height as f32;
            let t = sample_bicubic(scene.pano.as_raw(), gw, gh, gx, gy, true);
            for (c, tc) in t.iter().enumerate() {
                let d = pano.rgb[o * 3 + c] as f64 - *tc as f64;
                se += d * d;
            }
            n += 3;
        }
    }
    (10.0 * (255.0f64 * 255.0 / (se / n as f64)).log10(), covered as f64 / (pano.width * pano.height) as f64)
}

#[test]
fn fisheye_sphere_matches_ground_truth() {
    let mut lens = Lens::new(LensKind::Fisheye, 165.0, 640, 960);
    lens.b = -0.03;
    let poses = [(0.0, 3.0, 1.0), (88.0, -2.0, -1.5), (181.0, 2.0, 0.5), (272.0, -1.0, 2.0), (20.0, 87.0, 0.0), (-40.0, -88.0, 3.0)];
    let scene = build(lens, &poses, true, false);
    let est = scene.project.lenses[0];
    assert_eq!(est.kind, LensKind::Fisheye, "lens type should be detected from the images");
    assert!((est.hfov - 165.0).abs() < 1.0, "field of view {:.2}", est.hfov);
    let err = orientation_error_deg(&scene);
    assert!(err < 0.15, "orientation error {err:.3}°");
    let pano = render(&scene.project, &scene.shots, &RenderOptions { width: 1024, ..Default::default() }, &|_, _| {}).unwrap();
    assert!(pano.is_full_sphere());
    let (db, covered) = psnr(&scene, &pano);
    assert!(covered > 0.999, "coverage {covered}");
    assert!(db > 27.0, "PSNR {db:.1} dB");
}

#[test]
fn rectilinear_partial_panorama_matches_ground_truth() {
    let mut lens = Lens::new(LensKind::Rectilinear, 60.0, 900, 600);
    lens.b = -0.04;
    let poses = [
        (-50.0, 2.0, 0.5),
        (-25.0, -1.0, -1.0),
        (0.0, 1.0, 1.0),
        (25.0, 0.0, 0.0),
        (50.0, -2.0, -0.5),
        (-12.0, 28.0, 1.0),
        (14.0, 27.0, -1.0),
    ];
    let scene = build(lens, &poses, false, true);
    let err = orientation_error_deg(&scene);
    let est = scene.project.lenses[0];
    // A partial panorama only weakly constrains the focal length (it trades off against the
    // spacing of the shots), so absolute orientations carry a small scale uncertainty.
    assert!((est.hfov - 60.0).abs() < 1.5, "field of view {:.2}", est.hfov);
    assert!(err < 1.0, "orientation error {err:.3}°");
    let pano = render(&scene.project, &scene.shots, &RenderOptions::default(), &|_, _| {}).unwrap();
    assert!(!pano.is_full_sphere());
    let (hfov, vfov, _) = pano.coverage_degrees();
    assert!(hfov > 140.0 && hfov < 175.0 && vfov > 55.0 && vfov < 90.0, "coverage {hfov:.1}° × {vfov:.1}°");
    let (db, _) = psnr(&scene, &pano);
    assert!(db > 24.0, "PSNR {db:.1} dB");
}

#[test]
fn pole_gaps_are_filled_seamlessly() {
    use nodal_stitch::fill::{fill_poles, pole_gap, FillOptions};
    let truth = procedural_pano(1536, 5);
    let (w, h) = (truth.width() as usize, truth.height() as usize);
    let mut rgb = truth.as_raw().clone();
    let mut alpha = vec![255u8; w * h];
    let (zenith, nadir) = (18.0f32, 26.0f32);
    for y in 0..h {
        let colat = (y as f32 + 0.5) / h as f32 * 180.0;
        if colat < zenith || 180.0 - colat < nadir {
            alpha[y * w..(y + 1) * w].fill(0);
            rgb[y * w * 3..(y + 1) * w * 3].fill(0);
        }
    }
    let mut pano = nodal_stitch::Panorama {
        width: w,
        height: h,
        rgb,
        alpha,
        full_width: w,
        full_height: h,
        crop_x: 0,
        crop_y: 0,
        stats: Default::default(),
    };
    assert!((pole_gap(&pano, true, 62.0).unwrap() - zenith).abs() < 1.0);
    let report = fill_poles(&mut pano, &FillOptions::default(), &|_, _| {});
    assert!(report.zenith.is_some() && report.nadir.is_some(), "{report:?}");
    assert!(!pano.has_gaps(), "every pixel must be filled");
    // Untouched outside the patched caps (a few degrees of cross-fade allowed).
    for y in (h * 30 / 180)..(h * 145 / 180) {
        assert_eq!(&pano.rgb[y * w * 3..(y + 1) * w * 3], &truth.as_raw()[y * w * 3..(y + 1) * w * 3], "row {y} changed");
    }
    // The patch has the same overall tone and texture strength as the material around it.
    let stats = |img: &[u8], y0: usize, y1: usize| {
        let px = &img[y0 * w * 3..y1 * w * 3];
        let mean = px.iter().map(|v| *v as f64).sum::<f64>() / px.len() as f64;
        let var = px.iter().map(|v| (*v as f64 - mean).powi(2)).sum::<f64>() / px.len() as f64;
        (mean, var.sqrt())
    };
    for (y0, y1) in [(0, h * 16 / 180), (h * 156 / 180, h)] {
        let (m_fill, s_fill) = stats(&pano.rgb, y0, y1);
        let (m_true, s_true) = stats(truth.as_raw(), y0, y1);
        // The hidden cap can only be guessed, so this is a sanity bound, not an accuracy claim.
        assert!((m_fill - m_true).abs() < 35.0, "tone {m_fill:.1} vs {m_true:.1}");
        assert!(s_fill > s_true * 0.5 && s_fill < s_true * 1.6, "texture {s_fill:.1} vs {s_true:.1}");
    }
}

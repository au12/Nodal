//! Minimal command-line stitcher for testing: `stitch out.jpg [--width N] photos…`
use nodal_stitch::features::DetectOptions;
use nodal_stitch::{align_sources, io, load_sources, render, AlignOptions, RenderOptions};
use std::path::PathBuf;
use std::time::Instant;

fn main() -> anyhow::Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let out = PathBuf::from(args.remove(0));
    let mut width = 0;
    if args.first().is_some_and(|a| a == "--width") {
        width = args[1].parse()?;
        args.drain(0..2);
    }
    let paths: Vec<PathBuf> = args.iter().map(PathBuf::from).collect();
    let t0 = Instant::now();
    let sources = load_sources(&paths, &DetectOptions::default(), &|_, _| {})?;
    println!("loaded + features: {:.2?}", t0.elapsed());
    let t = Instant::now();
    let (project, report) = align_sources(&sources, &AlignOptions::default(), &|_, _| {})?;
    println!(
        "aligned: {:.2?}  {} pairs, {} points, RMS {:.2} px, max {:.2} px, unaligned {:?}",
        t.elapsed(),
        report.pairs.len(),
        report.control_points,
        report.rms,
        report.max,
        report.unaligned
    );
    for l in &report.log {
        println!("  {l}");
    }
    for l in &project.lenses {
        println!("  lens: {:?} hfov {:.2}° a {:.4} b {:.4} c {:.4} d {:.1} e {:.1}", l.kind, l.hfov, l.a, l.b, l.c, l.d, l.e);
    }
    let t = Instant::now();
    let images: Vec<_> = sources.iter().map(|s| s.image.clone()).collect();
    let pano = render(&project, &images, &RenderOptions { width, ..Default::default() }, &|_, _| {})?;
    println!("rendered {}x{} (full {}x{}) in {:.2?}", pano.width, pano.height, pano.full_width, pano.full_height, t.elapsed());
    for (name, secs) in &pano.stats.timings {
        println!("  {name}: {secs:.2}s");
    }
    println!("  gains: {:?}", pano.stats.gains.iter().map(|g| format!("{:.2}", g[1])).collect::<Vec<_>>());
    io::save_panorama(&pano, &out, 92)?;
    std::fs::write(out.with_extension("json"), project.to_json())?;
    println!("total {:.2?}", t0.elapsed());
    Ok(())
}

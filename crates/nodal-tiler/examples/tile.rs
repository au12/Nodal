//! cargo run --release -p nodal-tiler --example tile -- OUT_DIR IMAGE...
fn main() -> anyhow::Result<()> {
    let args: Vec<std::path::PathBuf> = std::env::args_os().skip(1).map(Into::into).collect();
    let (out, inputs) = args.split_first().expect("usage: tile OUT_DIR IMAGE...");
    let t0 = std::time::Instant::now();
    let tour = nodal_tiler::build_tour(inputs, out, Some("Demo"), &Default::default(), &|f, s| {
        eprint!("\r{:>5.1}% {s:<40}", f * 100.0);
    })?;
    eprintln!("\n{} scenes in {:.2?}", tour.scenes.len(), t0.elapsed());
    Ok(())
}

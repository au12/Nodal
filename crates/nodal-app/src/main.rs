//! `nodal` — native viewer/editor and command-line tools.

#[cfg(not(target_arch = "wasm32"))]
mod cli;

#[cfg(not(target_arch = "wasm32"))]
fn main() -> std::process::ExitCode {
    use clap::Parser;
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn,nodal=info,nodal_render=info,nodal_app=info")).init();
    let args = cli::Cli::parse();
    match args.command {
        Some(cmd) => cli::run(cmd),
        None => match gui(args.paths) {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("nodal: {e}");
                std::process::ExitCode::FAILURE
            }
        },
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn gui(paths: Vec<std::path::PathBuf>) -> eframe::Result<()> {
    let icon = eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon-256.png")).ok();
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("Nodal")
        .with_app_id("nodal")
        .with_inner_size([1280.0, 800.0])
        .with_min_inner_size([480.0, 360.0])
        .with_drag_and_drop(true);
    if let Some(icon) = icon {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions { viewport, renderer: eframe::Renderer::Wgpu, ..Default::default() };
    eframe::run_native(
        "Nodal",
        options,
        Box::new(move |cc| {
            let mut app = nodal_app::NodalApp::new(cc);
            if !paths.is_empty() {
                nodal_app::open_paths(&mut app, paths);
            }
            Ok(Box::new(app))
        }),
    )
}

#[cfg(target_arch = "wasm32")]
fn main() {}

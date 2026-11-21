//! cargo run --release -p nodal-render --example still -- TOUR_JSON OUT.png YAW PITCH FOV [DEBUG]
use nodal_core::{Tour, View};
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let tour_path = std::path::Path::new(&a[1]);
    let tour = Tour::from_json(&std::fs::read_to_string(tour_path).unwrap()).unwrap();
    let base = tour_path.parent().unwrap().to_str().unwrap().to_string();
    let mut hl = nodal_render::headless::Headless::new().unwrap();
    hl.engine.debug_mode = a.get(6).map(|s| s.parse().unwrap()).unwrap_or(0);
    let view = View { yaw: a[3].parse().unwrap(), pitch: a[4].parse().unwrap(), fov: a[5].parse().unwrap(), ..View::default() };
    let t = std::time::Instant::now();
    let img = hl.render(&base, &tour.scenes[0], [12, 13, 16], &view, 960, 600, std::time::Duration::from_secs(60)).unwrap();
    eprintln!("{} rendered in {:?}", hl.adapter, t.elapsed());
    img.save(&a[2]).unwrap();
}

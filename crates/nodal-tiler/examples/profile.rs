use nodal_core::cube::Face;
use std::time::Instant;
fn main() {
    let p = std::env::args().nth(1).unwrap();
    let t = Instant::now();
    let img = nodal_tiler::load_image(std::path::Path::new(&p)).unwrap();
    println!("decode {:?}", t.elapsed());
    let eq = nodal_tiler::sampler::Equirect::new(img, nodal_tiler::sampler::Coverage::FULL);
    for f in [Face::Front, Face::Up] {
        let t = Instant::now();
        let fi = nodal_tiler::face::render_face(&eq, f, 2608, 8);
        println!("render {:?} {:?}", f, t.elapsed());
        let t = Instant::now();
        let d = fi.downsample();
        println!("downsample {:?}", t.elapsed());
        let t = Instant::now();
        let mut n = 0;
        for y in 0..8 {
            for x in 0..8 {
                let px = fi.crop(x * 326 - 1, y * 326 - 1, 328);
                n += nodal_tiler::encode(&px, 328, 328, &Default::default()).unwrap().len();
            }
        }
        println!("encode 64 tiles (1 thread) {:?} -> {} KB", t.elapsed(), n / 1024);
        let _ = d;
    }
}

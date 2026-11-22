//! The live stitch preview compositor draws photos where their lens model says they are.

use nodal_render::headless::Headless;
use nodal_render::{ComposeFrame, ComposePhoto, Composer};
use std::f32::consts::PI;

fn photo(rot_rows: [[f32; 3]; 3], hfov_deg: f32, size: f32) -> ComposePhoto {
    // Rectilinear lens: focal length from the horizontal field of view.
    let f = size * 0.5 / (hfov_deg.to_radians() * 0.5).tan();
    ComposePhoto {
        r0: [rot_rows[0][0], rot_rows[0][1], rot_rows[0][2], 0.0],
        r1: [rot_rows[1][0], rot_rows[1][1], rot_rows[1][2], f],
        r2: [rot_rows[2][0], rot_rows[2][1], rot_rows[2][2], size * 0.5],
        centre: [size * 0.5, size * 0.5, size, size],
        poly: [0.0, 0.0, 0.0, 1.0],
        limits: [1e9, 88f32.to_radians(), 0.0, 0.0],
        gain: [1.0, 1.0, 1.0, 1.0],
    }
}

#[test]
fn photos_land_where_their_orientation_points() {
    let Ok(h) = Headless::new() else {
        eprintln!("no GPU adapter; skipping");
        return;
    };
    let mut composer = Composer::new(&h.device, nodal_render::wgpu::TextureFormat::Rgba8Unorm);
    // Two 64×64 photos: red looking forward, green looking 90° to the right.
    let red: Vec<u8> = [200u8, 30, 30, 255].repeat(64 * 64);
    let green: Vec<u8> = [30u8, 200, 30, 255].repeat(64 * 64);
    composer.set_layers(&h.device, &h.queue, 64, 64, &[&red, &green]);
    let identity = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
    // World → camera for a camera looking along +X (yaw 90°): camera x = −world z, camera z = world x.
    let right = [[0.0, 0.0, -1.0], [0.0, 1.0, 0.0], [1.0, 0.0, 0.0]];
    let frame = ComposeFrame {
        world: identity,
        rect: [0.0, 0.0, 256.0, 128.0],
        crop: [-PI, 2.0 * PI, PI / 2.0, PI],
        photos: vec![photo(identity, 60.0, 64.0), photo(right, 60.0, 64.0)],
    };
    let img = h.draw_and_read(256, 128, |view| composer.render_to(&h.device, &h.queue, view, &frame)).unwrap();
    let px = |x: u32, y: u32| img.get_pixel(x, y).0;
    // Canvas centre = forward → red photo. A quarter turn right (x = 192) → green photo.
    let c = px(128, 64);
    assert!(c[0] > 150 && c[1] < 80 && c[3] == 255, "centre {c:?}");
    let r = px(192, 64);
    assert!(r[1] > 150 && r[0] < 80 && r[3] == 255, "right {r:?}");
    // Behind the cameras nothing is drawn.
    assert_eq!(px(2, 64)[3], 0);
    // The photos span ±30° horizontally: just inside is covered, well outside is not.
    assert_eq!(px(128 + 18, 64)[3], 255);
    assert_eq!(px(128 + 32, 64)[3], 0);
}

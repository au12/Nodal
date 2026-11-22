// Live stitch preview: composites source photos onto an equirectangular canvas on the GPU.
//
// One fullscreen triangle. Every pixel becomes a world direction, is projected into each photo
// through its lens model (the same model the stitcher uses), and the photos that see it are
// feather-blended. Used for instant feedback while re-centring or levelling a panorama.

struct Photo {
    r0: vec4<f32>,      // world→camera rotation row 0 (xyz), projection factor k (w)
    r1: vec4<f32>,      // row 1 (xyz), focal length in pixels (w)
    r2: vec4<f32>,      // row 2 (xyz), normalisation radius in pixels (w)
    centre: vec4<f32>,  // principal point x, y; image width, height (pixels)
    poly: vec4<f32>,    // distortion a, b, c, d0
    limits: vec4<f32>,  // largest valid normalised radius, largest angle from the axis, unused, unused
    gain: vec4<f32>,    // linear RGB gain, enabled (w)
};

struct Uniforms {
    w0: vec4<f32>,      // rotation applied to canvas directions (rows), count in w0.w
    w1: vec4<f32>,      // encode sRGB (w)
    w2: vec4<f32>,
    rect: vec4<f32>,    // viewport origin px (xy), size px (zw)
    crop: vec4<f32>,    // yaw at the left edge, yaw span, pitch at the top edge, pitch span (radians)
    photos: array<Photo, 48>,
};

@group(0) @binding(0) var<uniform> u: Uniforms;
@group(0) @binding(1) var photos_tex: texture_2d_array<f32>;
@group(0) @binding(2) var samp: sampler;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
};

@vertex
fn vs_fullscreen(@builtin(vertex_index) vi: u32) -> VsOut {
    let x = f32((vi << 1u) & 2u);
    let y = f32(vi & 2u);
    var o: VsOut;
    o.pos = vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
    return o;
}

// Ideal radius in focal lengths for the one-parameter projection family (PTGui's fisheye
// factor): k = 1 rectilinear, 0.5 stereographic, 0 equidistant, -0.5 equisolid.
fn ideal_radius(k: f32, theta: f32) -> f32 {
    if (abs(k) < 1e-6) {
        return theta;
    } else if (k > 0.0) {
        return tan(k * theta) / k;
    }
    return sin(k * theta) / k;
}

fn to_srgb(c: vec3<f32>) -> vec3<f32> {
    let lo = c * 12.92;
    let hi = 1.055 * pow(max(c, vec3<f32>(0.0)), vec3<f32>(1.0 / 2.4)) - 0.055;
    return select(hi, lo, c <= vec3<f32>(0.0031308));
}

@fragment
fn fs_compose(in: VsOut) -> @location(0) vec4<f32> {
    let uv = (in.pos.xy - u.rect.xy) / u.rect.zw;
    let yaw = u.crop.x + uv.x * u.crop.y;
    let pitch = u.crop.z - uv.y * u.crop.w;
    let cp = cos(pitch);
    let canvas_dir = vec3<f32>(sin(yaw) * cp, sin(pitch), cos(yaw) * cp);
    let dir = vec3<f32>(dot(u.w0.xyz, canvas_dir), dot(u.w1.xyz, canvas_dir), dot(u.w2.xyz, canvas_dir));
    let count = u32(u.w0.w);
    var acc = vec3<f32>(0.0);
    var wsum = 0.0;
    for (var i = 0u; i < count; i = i + 1u) {
        let p = u.photos[i];
        if (p.gain.w < 0.5) {
            continue;
        }
        let cam = vec3<f32>(dot(p.r0.xyz, dir), dot(p.r1.xyz, dir), dot(p.r2.xyz, dir));
        let rxy = length(cam.xy);
        let theta = atan2(rxy, cam.z);
        if (theta > p.limits.y) {
            continue;
        }
        let rho = p.r1.w * ideal_radius(p.r0.w, theta) / p.r2.w;
        if (rho > p.limits.x) {
            continue;
        }
        let r_src = rho * (((p.poly.x * rho + p.poly.y) * rho + p.poly.z) * rho + p.poly.w) * p.r2.w;
        let s = r_src / max(rxy, 1e-9);
        let px = vec2<f32>(p.centre.x + cam.x * s, p.centre.y - cam.y * s);
        let size = p.centre.zw;
        if (px.x < 1.0 || px.y < 1.0 || px.x > size.x - 1.0 || px.y > size.y - 1.0) {
            continue;
        }
        // Feather: weight falls to zero at the photo's edge, so overlaps cross-fade.
        let edge = min(min(px.x, size.x - px.x), min(px.y, size.y - px.y)) / (0.5 * min(size.x, size.y));
        let wgt = edge * edge * edge + 1e-5;
        let c = textureSampleLevel(photos_tex, samp, px / size, i32(i), 0.0).rgb * p.gain.rgb;
        acc += c * wgt;
        wsum += wgt;
    }
    if (wsum <= 0.0) {
        return vec4<f32>(0.0, 0.0, 0.0, 0.0);
    }
    var rgb = acc / wsum;
    if (u.w1.w > 0.5) {
        rgb = to_srgb(rgb);
    }
    return vec4<f32>(rgb, 1.0);
}

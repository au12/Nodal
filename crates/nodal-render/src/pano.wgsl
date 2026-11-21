// Nodal panorama shader.
//
// One fullscreen triangle. Every pixel:
//   1. inverts the active projection to get a world direction (exact, no tessellation),
//   2. picks the cube face and face uv,
//   3. derives the ideal level of detail from screen-space derivatives of the direction
//      (continuous across face seams, unlike uv derivatives),
//   4. resolves the two nearest levels through the page table and blends them (trilinear).
//
// Must mirror nodal-core's projection.rs and cube.rs.

struct Uniforms {
    basis_r: vec4<f32>,   // camera right (xyz), projection mode (w: 0 perspective, 1 pannini, 2 equirect)
    basis_u: vec4<f32>,   // camera up (xyz), distortion d (w)
    basis_f: vec4<f32>,   // camera forward (xyz), plane scale k (w)
    proj: vec4<f32>,      // architectural shift, half extent px, yaw, pitch
    screen: vec4<f32>,    // viewport origin px (xy), viewport size px (zw)
    misc: vec4<f32>,      // half fov, lod bias, encode sRGB (0/1), base ready (0/1)
    pyr: vec4<f32>,       // tile size, border, levels, slot size
    atlas: vec4<f32>,     // atlas size px, slots per row, base max lod, debug mode
    bg: vec4<f32>,        // background (linear rgb)
};

@group(0) @binding(0) var<uniform> u: Uniforms;
@group(0) @binding(1) var base_tex: texture_2d_array<f32>;
@group(0) @binding(2) var atlas_tex: texture_2d<f32>;
@group(0) @binding(3) var page_tex: texture_2d_array<u32>;
@group(0) @binding(4) var samp: sampler;

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

const HALF_PI: f32 = 1.5707963;

fn persp_theta(r: f32, d: f32) -> f32 {
    let s = d + 1.0;
    return atan2(r, s) + asin(clamp(r * d / sqrt(s * s + r * r), -1.0, 1.0));
}

// World direction (xyz) and validity (w) for a viewport pixel.
fn screen_to_dir(p: vec2<f32>) -> vec4<f32> {
    let half = u.proj.y;
    let n = vec2<f32>((p.x - u.screen.z * 0.5) / half, -(p.y - u.screen.w * 0.5) / half);
    let mode = u.basis_r.w;
    let d = u.basis_u.w;
    let k = u.basis_f.w;
    if (mode > 1.5) {
        let hf = u.misc.x;
        let lat = n.y * hf + u.proj.w;
        let lon = u.proj.z + n.x * hf;
        let la = clamp(lat, -HALF_PI, HALF_PI);
        let valid = select(0.0, 1.0, abs(lat) <= HALF_PI);
        return vec4<f32>(sin(lon) * cos(la), sin(la), cos(lon) * cos(la), valid);
    }
    var ray: vec3<f32>;
    if (mode > 0.5) {
        let q = n * k;
        let phi = persp_theta(q.x, d);
        let s = (d + 1.0) / (d + cos(phi));
        ray = normalize(vec3<f32>(sin(phi), q.y / s, cos(phi)));
    } else {
        let q = vec2<f32>(n.x * k, n.y * k + u.proj.x);
        let r = length(q);
        let th = persp_theta(r, d);
        let qn = select(vec2<f32>(0.0, 0.0), q / max(r, 1e-12), r > 1e-9);
        ray = vec3<f32>(sin(th) * qn, cos(th));
    }
    let dir = u.basis_r.xyz * ray.x + u.basis_u.xyz * ray.y + u.basis_f.xyz * ray.z;
    return vec4<f32>(dir, 1.0);
}

fn face_of(d: vec3<f32>) -> u32 {
    let a = abs(d);
    if (a.x >= a.y && a.x >= a.z) {
        return select(3u, 1u, d.x > 0.0);
    }
    if (a.y >= a.z) {
        return select(5u, 4u, d.y > 0.0);
    }
    return select(2u, 0u, d.z > 0.0);
}

// Columns: face centre, right axis, down axis.
fn face_frame(f: u32) -> mat3x3<f32> {
    switch f {
        case 0u: { return mat3x3<f32>(vec3<f32>(0.0, 0.0, 1.0), vec3<f32>(1.0, 0.0, 0.0), vec3<f32>(0.0, -1.0, 0.0)); }
        case 1u: { return mat3x3<f32>(vec3<f32>(1.0, 0.0, 0.0), vec3<f32>(0.0, 0.0, -1.0), vec3<f32>(0.0, -1.0, 0.0)); }
        case 2u: { return mat3x3<f32>(vec3<f32>(0.0, 0.0, -1.0), vec3<f32>(-1.0, 0.0, 0.0), vec3<f32>(0.0, -1.0, 0.0)); }
        case 3u: { return mat3x3<f32>(vec3<f32>(-1.0, 0.0, 0.0), vec3<f32>(0.0, 0.0, 1.0), vec3<f32>(0.0, -1.0, 0.0)); }
        case 4u: { return mat3x3<f32>(vec3<f32>(0.0, 1.0, 0.0), vec3<f32>(1.0, 0.0, 0.0), vec3<f32>(0.0, 0.0, 1.0)); }
        default: { return mat3x3<f32>(vec3<f32>(0.0, -1.0, 0.0), vec3<f32>(1.0, 0.0, 0.0), vec3<f32>(0.0, 0.0, -1.0)); }
    }
}

fn sample_base(f: u32, uv: vec2<f32>, lod: f32) -> vec3<f32> {
    let px = u.pyr.y + uv * u.pyr.x;
    return textureSampleLevel(base_tex, samp, px / u.pyr.w, i32(f), lod).rgb;
}

fn sample_atlas(slot: u32, level: u32, uv: vec2<f32>) -> vec3<f32> {
    let n = f32(1u << level);
    let tc = uv * n;
    let txy = clamp(floor(tc), vec2<f32>(0.0, 0.0), vec2<f32>(n - 1.0, n - 1.0));
    let fr = clamp(tc - txy, vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 1.0));
    let per_row = u32(u.atlas.y);
    let sxy = vec2<f32>(f32(slot % per_row), f32(slot / per_row));
    let px = sxy * u.pyr.w + vec2<f32>(u.pyr.y, u.pyr.y) + fr * u.pyr.x;
    return textureSampleLevel(atlas_tex, samp, px / u.atlas.x, 0.0).rgb;
}

fn sample_level(level: u32, f: u32, uv: vec2<f32>, minify: f32) -> vec3<f32> {
    if (level == 0u) {
        return sample_base(f, uv, minify);
    }
    let levels = u32(u.pyr.z);
    let n = 1u << level;
    let ti = clamp(vec2<i32>(floor(uv * f32(n))), vec2<i32>(0, 0), vec2<i32>(i32(n) - 1, i32(n) - 1));
    let mip = i32(levels - 1u - level);
    let e = textureLoad(page_tex, ti, i32(f), mip).r;
    if ((e & 0x80000000u) == 0u) {
        return sample_base(f, uv, 0.0);
    }
    return sample_atlas(e & 0xFFFFFFu, (e >> 24u) & 0x7Fu, uv);
}

fn linear_to_srgb(c: vec3<f32>) -> vec3<f32> {
    let lo = c * 12.92;
    let hi = 1.055 * pow(max(c, vec3<f32>(0.0, 0.0, 0.0)), vec3<f32>(1.0 / 2.4, 1.0 / 2.4, 1.0 / 2.4)) - 0.055;
    return select(hi, lo, c <= vec3<f32>(0.0031308, 0.0031308, 0.0031308));
}

@fragment
fn fs_pano(in: VsOut) -> @location(0) vec4<f32> {
    let p = in.pos.xy - u.screen.xy;
    let sd = screen_to_dir(p);
    let dir = sd.xyz;
    // Derivatives must be taken in uniform control flow, before any branching on pixel data.
    let ddx = dpdx(dir);
    let ddy = dpdy(dir);
    var col = u.bg.rgb;
    if (sd.w > 0.5 && u.misc.w > 0.5) {
        let f = face_of(dir);
        let fr = face_frame(f);
        let c = fr[0];
        let r = fr[1];
        let dn = fr[2];
        let dc = dot(dir, c);
        let pp = dir / dc;
        let uv = vec2<f32>(dot(pp, r), dot(pp, dn)) * 0.5 + vec2<f32>(0.5, 0.5);
        // d(plane point)/d(screen) via the quotient rule; stays continuous across faces.
        let inv = 1.0 / (dc * dc);
        let dpx = (ddx * dc - dir * dot(ddx, c)) * inv;
        let dpy = (ddy * dc - dir * dot(ddy, c)) * inv;
        let gx = vec2<f32>(dot(dpx, r), dot(dpx, dn)) * 0.5;
        let gy = vec2<f32>(dot(dpy, r), dot(dpy, dn)) * 0.5;
        let fp = max(length(gx), length(gy)) * u.pyr.x;
        var lf = -log2(max(fp, 1e-8)) + u.misc.y;
        if (!(lf > -64.0 && lf < 64.0)) {
            lf = 0.0;
        }
        let lc = clamp(lf, 0.0, u.pyr.z - 1.0);
        let l0 = floor(lc);
        let t = lc - l0;
        let minify = clamp(-lf, 0.0, u.atlas.z);
        let dbg = u32(u.atlas.w + 0.5);
        if (dbg == 1u) {
            return vec4<f32>(uv, f32(f) / 5.0, 1.0);
        } else if (dbg == 2u) {
            // Level heat map: blue (base) → red (finest), stripes show the trilinear blend.
            let x = lc / max(u.pyr.z - 1.0, 1.0);
            col = vec3<f32>(x, 1.0 - abs(x - 0.5) * 2.0, 1.0 - x) * (0.75 + 0.25 * t);
            col = col * col;
        } else if (dbg == 3u) {
            col = sample_base(f, uv, minify);
        } else {
            col = sample_level(u32(l0), f, uv, minify);
            if (t > 0.002) {
                col = mix(col, sample_level(u32(l0) + 1u, f, uv, minify), t);
            }
        }
    }
    if (u.misc.z > 0.5) {
        col = linear_to_srgb(col);
    }
    return vec4<f32>(col, 1.0);
}

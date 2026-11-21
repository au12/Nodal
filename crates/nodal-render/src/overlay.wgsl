// Transition overlay: draws a snapshot of the previous scene on top of the new one,
// scaled about a focus point (zoom-through effect) and faded out.

struct Overlay {
    rect: vec4<f32>,   // viewport origin px (xy), viewport size px (zw)
    xf: vec4<f32>,     // focus point px (xy, viewport-relative), scale (z), opacity (w)
};

@group(0) @binding(0) var<uniform> o: Overlay;
@group(0) @binding(1) var snap: texture_2d<f32>;
@group(0) @binding(2) var snap_samp: sampler;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
};

@vertex
fn vs_fullscreen(@builtin(vertex_index) vi: u32) -> VsOut {
    let x = f32((vi << 1u) & 2u);
    let y = f32(vi & 2u);
    var out: VsOut;
    out.pos = vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
    return out;
}

@fragment
fn fs_overlay(in: VsOut) -> @location(0) vec4<f32> {
    let p = in.pos.xy - o.rect.xy;
    let src = (p - o.xf.xy) / o.xf.z + o.xf.xy;
    let uv = src / o.rect.zw;
    let c = textureSampleLevel(snap, snap_samp, clamp(uv, vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 1.0)), 0.0);
    let inside = all(uv >= vec2<f32>(0.0, 0.0)) && all(uv <= vec2<f32>(1.0, 1.0));
    return vec4<f32>(c.rgb, select(0.0, o.xf.w, inside));
}

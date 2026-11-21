//! GPU side: pipelines, per-scene textures, uploads and drawing.

use crate::cache::TileCache;
use crate::fetch::TileImage;
use crate::stream::{compute_wants, Want};
use bytemuck::{Pod, Zeroable};
use nodal_core::pyramid::{Pyramid, TileId};
use nodal_core::{ProjMode, Projector};
use std::collections::HashMap;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod, Zeroable)]
pub struct PanoUniforms {
    pub basis_r: [f32; 4],
    pub basis_u: [f32; 4],
    pub basis_f: [f32; 4],
    pub proj: [f32; 4],
    pub screen: [f32; 4],
    pub misc: [f32; 4],
    pub pyr: [f32; 4],
    pub atlas: [f32; 4],
    pub bg: [f32; 4],
}

impl PanoUniforms {
    /// Moves the viewport origin (physical pixels). Used when the paint rect is only known at draw time.
    pub fn with_origin(mut self, x: f32, y: f32) -> Self {
        self.screen[0] = x;
        self.screen[1] = y;
        self
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod, Zeroable, PartialEq)]
pub struct OverlayUniforms {
    pub rect: [f32; 4],
    pub xf: [f32; 4],
}

#[derive(Clone, Copy, Debug, Default)]
pub struct EngineStats {
    pub resident: usize,
    pub capacity: usize,
    pub base_ready: bool,
    pub wanted_on_screen: usize,
    pub missing_on_screen: usize,
    pub uploads: usize,
}

pub struct SceneGpu {
    pub generation: u32,
    pub pyramid: Pyramid,
    pub(crate) base: wgpu::Texture,
    base_mips: u32,
    atlas: wgpu::Texture,
    atlas_size: u32,
    per_row: u32,
    page: wgpu::Texture,
    uniform_buf: wgpu::Buffer,
    snap_uniform_buf: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    snap_bind_group: wgpu::BindGroup,
    pub cache: TileCache,
    base_loaded: [bool; 6],
    pub base_ready: bool,
    background: [f32; 3],
}

impl SceneGpu {
    pub fn is_resident(&self, t: &TileId) -> bool {
        if t.level == 0 {
            self.base_loaded[t.face as usize]
        } else {
            self.cache.is_resident(t)
        }
    }
}

struct Snapshot {
    _texture: wgpu::Texture,
    view: wgpu::TextureView,
    bind_group: wgpu::BindGroup,
    size: (u32, u32),
}

pub struct PanoEngine {
    target_format: wgpu::TextureFormat,
    encode_srgb: bool,
    pano_bgl: wgpu::BindGroupLayout,
    pano_pipeline: wgpu::RenderPipeline,
    overlay_bgl: wgpu::BindGroupLayout,
    overlay_pipeline: wgpu::RenderPipeline,
    overlay_buf: wgpu::Buffer,
    sampler: wgpu::Sampler,
    max_texture: u32,
    /// Requested atlas edge in pixels (0 = automatic).
    pub atlas_target: u32,
    pub scene: Option<SceneGpu>,
    snapshot: Option<Snapshot>,
    overlay_active: bool,
    wants_scratch: HashMap<TileId, f32>,
    pub wants: Vec<Want>,
    pub stats: EngineStats,
    /// Shader debug view: 0 normal, 1 face uv, 2 level-of-detail heat map, 3 base layer only.
    pub debug_mode: u32,
}

const SCENE_TEX: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;
/// Six cube faces plus one spare layer (see `set_scene`).
const LAYERS: u32 = 7;

fn uniform_entry(binding: u32, size: u64) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: false,
            min_binding_size: wgpu::BufferSize::new(size),
        },
        count: None,
    }
}

fn texture_entry(binding: u32, dim: wgpu::TextureViewDimension, sample_type: wgpu::TextureSampleType) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture { sample_type, view_dimension: dim, multisampled: false },
        count: None,
    }
}

fn sampler_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
        count: None,
    }
}

fn pipeline(
    device: &wgpu::Device,
    label: &str,
    module: &wgpu::ShaderModule,
    fs: &str,
    bgl: &wgpu::BindGroupLayout,
    format: wgpu::TextureFormat,
    blend: Option<wgpu::BlendState>,
) -> wgpu::RenderPipeline {
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some(label),
        bind_group_layouts: &[Some(bgl)],
        immediate_size: 0,
    });
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label),
        layout: Some(&layout),
        vertex: wgpu::VertexState { module, entry_point: Some("vs_fullscreen"), compilation_options: Default::default(), buffers: &[] },
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module,
            entry_point: Some(fs),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState { format, blend, write_mask: wgpu::ColorWrites::ALL })],
        }),
        multiview_mask: None,
        cache: None,
    })
}

impl PanoEngine {
    pub fn new(device: &wgpu::Device, target_format: wgpu::TextureFormat) -> Self {
        let pano_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("nodal pano"),
            source: wgpu::ShaderSource::Wgsl(include_str!("pano.wgsl").into()),
        });
        let overlay_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("nodal overlay"),
            source: wgpu::ShaderSource::Wgsl(include_str!("overlay.wgsl").into()),
        });
        use wgpu::TextureSampleType as S;
        use wgpu::TextureViewDimension as D;
        let pano_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("nodal pano bgl"),
            entries: &[
                uniform_entry(0, std::mem::size_of::<PanoUniforms>() as u64),
                texture_entry(1, D::D2Array, S::Float { filterable: true }),
                texture_entry(2, D::D2, S::Float { filterable: true }),
                texture_entry(3, D::D2Array, S::Uint),
                sampler_entry(4),
            ],
        });
        let overlay_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("nodal overlay bgl"),
            entries: &[
                uniform_entry(0, std::mem::size_of::<OverlayUniforms>() as u64),
                texture_entry(1, D::D2, S::Float { filterable: true }),
                sampler_entry(2),
            ],
        });
        let pano_pipeline = pipeline(device, "nodal pano", &pano_module, "fs_pano", &pano_bgl, target_format, None);
        let overlay_pipeline = pipeline(
            device,
            "nodal overlay",
            &overlay_module,
            "fs_overlay",
            &overlay_bgl,
            target_format,
            Some(wgpu::BlendState::ALPHA_BLENDING),
        );
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("nodal linear"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            ..Default::default()
        });
        let overlay_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("nodal overlay uniforms"),
            size: std::mem::size_of::<OverlayUniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Self {
            target_format,
            encode_srgb: !target_format.is_srgb(),
            pano_bgl,
            pano_pipeline,
            overlay_bgl,
            overlay_pipeline,
            overlay_buf,
            sampler,
            max_texture: device.limits().max_texture_dimension_2d,
            atlas_target: 0,
            scene: None,
            snapshot: None,
            overlay_active: false,
            wants_scratch: HashMap::new(),
            wants: Vec::new(),
            stats: EngineStats::default(),
            debug_mode: 0,
        }
    }

    pub fn target_format(&self) -> wgpu::TextureFormat {
        self.target_format
    }

    /// Replaces the active scene. All tiles of the previous scene are dropped.
    pub fn set_scene(&mut self, device: &wgpu::Device, generation: u32, pyramid: Pyramid, background_srgb: [u8; 3]) {
        let slot = pyramid.slot_size();
        let base_mips = 32 - slot.leading_zeros(); // floor(log2(slot)) + 1
        let base = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("nodal base"),
            // 7 layers, not 6: wgpu's GL backend treats 2D textures with exactly 6 layers as cube maps.
            size: wgpu::Extent3d { width: slot, height: slot, depth_or_array_layers: LAYERS },
            mip_level_count: base_mips,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: SCENE_TEX,
            usage: wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::COPY_SRC
                | wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let desired = if self.atlas_target > 0 {
            self.atlas_target
        } else if cfg!(target_arch = "wasm32") {
            4096
        } else {
            6144
        };
        let atlas_edge = desired.min(self.max_texture).max(slot);
        let per_row = (atlas_edge / slot).max(1);
        let atlas_size = per_row * slot;
        let atlas = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("nodal atlas"),
            size: wgpu::Extent3d { width: atlas_size, height: atlas_size, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: SCENE_TEX,
            // RENDER_ATTACHMENT is required by copyExternalImageToTexture on WebGPU.
            usage: wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::COPY_SRC
                | wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let detail_levels = pyramid.levels.saturating_sub(1).max(1);
        let page_edge = if pyramid.levels > 1 { 1u32 << (pyramid.levels - 1) } else { 1 };
        let page = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("nodal page table"),
            size: wgpu::Extent3d { width: page_edge, height: page_edge, depth_or_array_layers: LAYERS },
            mip_level_count: detail_levels,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R32Uint,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let make_ubuf = |label| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: std::mem::size_of::<PanoUniforms>() as u64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            })
        };
        let uniform_buf = make_ubuf("nodal uniforms");
        let snap_uniform_buf = make_ubuf("nodal snapshot uniforms");
        let base_view =
            base.create_view(&wgpu::TextureViewDescriptor { dimension: Some(wgpu::TextureViewDimension::D2Array), ..Default::default() });
        let atlas_view = atlas.create_view(&Default::default());
        let page_view =
            page.create_view(&wgpu::TextureViewDescriptor { dimension: Some(wgpu::TextureViewDimension::D2Array), ..Default::default() });
        let make_bg = |buf: &wgpu::Buffer| {
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("nodal pano bg"),
                layout: &self.pano_bgl,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: buf.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&base_view) },
                    wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(&atlas_view) },
                    wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::TextureView(&page_view) },
                    wgpu::BindGroupEntry { binding: 4, resource: wgpu::BindingResource::Sampler(&self.sampler) },
                ],
            })
        };
        let bind_group = make_bg(&uniform_buf);
        let snap_bind_group = make_bg(&snap_uniform_buf);
        let bg = background_srgb.map(|c| nodal_core::math::srgb_to_linear(c as f32 / 255.0));
        let cache = TileCache::new(pyramid.levels, per_row * per_row);
        log::info!(
            "scene {generation}: {} levels, tile {}px, atlas {}px ({} slots)",
            pyramid.levels,
            pyramid.tile_size,
            atlas_size,
            per_row * per_row
        );
        self.scene = Some(SceneGpu {
            generation,
            pyramid,
            base,
            base_mips,
            atlas,
            atlas_size,
            per_row,
            page,
            uniform_buf,
            snap_uniform_buf,
            bind_group,
            snap_bind_group,
            cache,
            base_loaded: [false; 6],
            base_ready: false,
            background: bg,
        });
    }

    pub fn generation(&self) -> Option<u32> {
        self.scene.as_ref().map(|s| s.generation)
    }

    /// Computes the wanted tiles for this view, protects the visible resident ones from
    /// eviction and returns the list (most urgent first).
    pub fn plan(&mut self, proj: &Projector, lod_bias: f32) -> &[Want] {
        let Some(scene) = &mut self.scene else {
            self.wants.clear();
            return &self.wants;
        };
        compute_wants(proj, &scene.pyramid, lod_bias, &mut self.wants_scratch, &mut self.wants);
        scene.cache.begin_frame();
        // Never want more detail tiles than fit in the atlas: drop the least urgent.
        let cap = (scene.cache.slot_count as usize * 9 / 10).max(1);
        let mut detail = 0usize;
        self.wants.retain(|w| {
            if w.tile.level == 0 {
                return true;
            }
            detail += 1;
            detail <= cap
        });
        let mut on = 0;
        let mut missing = 0;
        for w in &self.wants {
            if w.tile.level > 0 {
                scene.cache.touch(&w.tile);
            }
            if w.on_screen() {
                on += 1;
                if !scene.is_resident(&w.tile) {
                    missing += 1;
                }
            }
        }
        self.stats.wanted_on_screen = on;
        self.stats.missing_on_screen = missing;
        self.stats.resident = scene.cache.resident_count();
        self.stats.capacity = scene.cache.slot_count as usize;
        self.stats.base_ready = scene.base_ready;
        &self.wants
    }

    /// Uploads a decoded tile. Returns false if it was stale or did not fit.
    pub fn accept_tile(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, generation: u32, tile: TileId, img: TileImage) -> bool {
        let Some(scene) = &mut self.scene else { return false };
        if scene.generation != generation {
            return false;
        }
        let slot_px = scene.pyramid.slot_size();
        let (w, h) = img.size();
        if w != slot_px || h != slot_px {
            log::warn!("tile {tile:?} is {w}×{h}, expected {slot_px}²");
            return false;
        }
        self.stats.uploads += 1;
        if tile.level == 0 {
            // The base layer is mipmapped on the CPU (a few hundred KB per face): portable to
            // WebGL 2, which cannot render into single layers through texture views.
            let TileImage::Rgba { data, .. } = img;
            let mut level = data;
            let mut size = slot_px;
            for mip in 0..scene.base_mips {
                write_rgba(queue, &scene.base, mip, wgpu::Origin3d { x: 0, y: 0, z: tile.face as u32 }, size, size, &level);
                if mip + 1 < scene.base_mips {
                    level = downsample_rgba(&level, size);
                    size = (size / 2).max(1);
                }
            }
            scene.base_loaded[tile.face as usize] = true;
            if !scene.base_ready && scene.base_loaded.iter().all(|b| *b) {
                scene.base_ready = true;
            }
            let _ = device;
            return true;
        }
        let Some(slot) = scene.cache.allocate(tile) else { return false };
        let origin = wgpu::Origin3d { x: (slot % scene.per_row) * slot_px, y: (slot / scene.per_row) * slot_px, z: 0 };
        upload(queue, &scene.atlas, 0, origin, img);
        true
    }

    /// Writes changed page-table slices to the GPU.
    pub fn flush(&mut self, queue: &wgpu::Queue) {
        let Some(scene) = &mut self.scene else { return };
        let levels = scene.pyramid.levels;
        let page = &scene.page;
        for (face, level, entries) in scene.cache.take_dirty() {
            let n = 1u32 << level;
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: page,
                    mip_level: levels - 1 - level,
                    origin: wgpu::Origin3d { x: 0, y: 0, z: face },
                    aspect: wgpu::TextureAspect::All,
                },
                bytemuck::cast_slice(entries),
                wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(4 * n), rows_per_image: Some(n) },
                wgpu::Extent3d { width: n, height: n, depth_or_array_layers: 1 },
            );
        }
    }

    pub fn uniforms(&self, proj: &Projector, origin: [f32; 2], lod_bias: f32) -> PanoUniforms {
        let mode = match proj.mode {
            ProjMode::Perspective => 0.0,
            ProjMode::Pannini => 1.0,
            ProjMode::Equirect => 2.0,
        };
        let b = proj.basis;
        let (pyr, atlas, ready, bg) = match &self.scene {
            Some(s) => (
                [s.pyramid.tile_size as f32, s.pyramid.border as f32, s.pyramid.levels as f32, s.pyramid.slot_size() as f32],
                [s.atlas_size as f32, s.per_row as f32, (s.base_mips - 1) as f32, self.debug_mode as f32],
                if s.base_ready { 1.0 } else { 0.0 },
                s.background,
            ),
            None => ([1.0, 0.0, 1.0, 1.0], [1.0, 1.0, 0.0, 0.0], 0.0, [0.0; 3]),
        };
        PanoUniforms {
            basis_r: [b.x_axis.x, b.x_axis.y, b.x_axis.z, mode],
            basis_u: [b.y_axis.x, b.y_axis.y, b.y_axis.z, proj.d],
            basis_f: [b.z_axis.x, b.z_axis.y, b.z_axis.z, proj.k],
            proj: [proj.shift, proj.half, proj.yaw, proj.pitch],
            screen: [origin[0], origin[1], proj.width, proj.height],
            misc: [proj.half_fov, lod_bias, if self.encode_srgb { 1.0 } else { 0.0 }, ready],
            pyr,
            atlas,
            bg: [bg[0], bg[1], bg[2], 1.0],
        }
    }

    /// Per-frame uniform upload (call from the paint callback's `prepare`).
    pub fn write_uniforms(&self, queue: &wgpu::Queue, u: &PanoUniforms, overlay: Option<OverlayUniforms>) {
        if let Some(s) = &self.scene {
            queue.write_buffer(&s.uniform_buf, 0, bytemuck::bytes_of(u));
        }
        if let Some(o) = overlay {
            queue.write_buffer(&self.overlay_buf, 0, bytemuck::bytes_of(&o));
        }
    }

    pub fn set_overlay_active(&mut self, active: bool) {
        self.overlay_active = active && self.snapshot.is_some();
    }

    pub fn has_snapshot(&self) -> bool {
        self.snapshot.is_some()
    }

    pub fn drop_snapshot(&mut self) {
        self.snapshot = None;
        self.overlay_active = false;
    }

    /// Draws the panorama (and the transition overlay, if active) into the current pass.
    pub fn paint(&self, rpass: &mut wgpu::RenderPass<'_>) {
        if let Some(scene) = &self.scene {
            rpass.set_pipeline(&self.pano_pipeline);
            rpass.set_bind_group(0, &scene.bind_group, &[]);
            rpass.draw(0..3, 0..1);
        }
        if self.overlay_active {
            if let Some(snap) = &self.snapshot {
                rpass.set_pipeline(&self.overlay_pipeline);
                rpass.set_bind_group(0, &snap.bind_group, &[]);
                rpass.draw(0..3, 0..1);
            }
        }
    }

    /// Renders the current scene into the transition snapshot texture.
    pub fn capture_snapshot(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, proj: &Projector, lod_bias: f32) {
        let Some(scene) = &self.scene else { return };
        let size = (proj.width.round().max(1.0) as u32, proj.height.round().max(1.0) as u32);
        if self.snapshot.as_ref().map(|s| s.size) != Some(size) {
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("nodal snapshot"),
                size: wgpu::Extent3d { width: size.0, height: size.1, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: self.target_format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            });
            let view = texture.create_view(&Default::default());
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("nodal overlay bg"),
                layout: &self.overlay_bgl,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: self.overlay_buf.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&view) },
                    wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(&self.sampler) },
                ],
            });
            self.snapshot = Some(Snapshot { _texture: texture, view, bind_group, size });
        }
        let u = self.uniforms(proj, [0.0, 0.0], lod_bias);
        queue.write_buffer(&scene.snap_uniform_buf, 0, bytemuck::bytes_of(&u));
        let snap = self.snapshot.as_ref().unwrap();
        let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("nodal snapshot") });
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("nodal snapshot"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &snap.view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.pano_pipeline);
            pass.set_bind_group(0, &scene.snap_bind_group, &[]);
            pass.draw(0..3, 0..1);
        }
        queue.submit([enc.finish()]);
    }

    /// Renders the current scene into an arbitrary view of `target_format` (offscreen rendering).
    pub fn render_to(&self, device: &wgpu::Device, queue: &wgpu::Queue, target: &wgpu::TextureView, proj: &Projector, lod_bias: f32) {
        let Some(scene) = &self.scene else { return };
        let u = self.uniforms(proj, [0.0, 0.0], lod_bias);
        queue.write_buffer(&scene.uniform_buf, 0, bytemuck::bytes_of(&u));
        let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("nodal offscreen") });
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("nodal offscreen"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            self.paint(&mut pass);
        }
        queue.submit([enc.finish()]);
    }
}

fn upload(queue: &wgpu::Queue, texture: &wgpu::Texture, mip: u32, origin: wgpu::Origin3d, img: TileImage) {
    let TileImage::Rgba { width, height, data } = img;
    write_rgba(queue, texture, mip, origin, width, height, &data);
}

fn write_rgba(queue: &wgpu::Queue, texture: &wgpu::Texture, mip: u32, origin: wgpu::Origin3d, w: u32, h: u32, data: &[u8]) {
    queue.write_texture(
        wgpu::TexelCopyTextureInfo { texture, mip_level: mip, origin, aspect: wgpu::TextureAspect::All },
        data,
        wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(4 * w), rows_per_image: Some(h) },
        wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
    );
}

/// 2×2 box filter of a square RGBA image (odd sizes drop the last row/column).
fn downsample_rgba(src: &[u8], size: u32) -> Vec<u8> {
    let n = (size / 2).max(1) as usize;
    let s = size as usize;
    let mut out = vec![0u8; n * n * 4];
    for y in 0..n {
        for x in 0..n {
            let (x0, y0) = ((2 * x).min(s - 1), (2 * y).min(s - 1));
            let (x1, y1) = ((2 * x + 1).min(s - 1), (2 * y + 1).min(s - 1));
            for c in 0..4 {
                let v = src[(y0 * s + x0) * 4 + c] as u32
                    + src[(y0 * s + x1) * 4 + c] as u32
                    + src[(y1 * s + x0) * 4 + c] as u32
                    + src[(y1 * s + x1) * 4 + c] as u32;
                out[(y * n + x) * 4 + c] = ((v + 2) / 4) as u8;
            }
        }
    }
    out
}

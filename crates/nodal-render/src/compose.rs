//! GPU compositor for the live stitch preview (see `compose.wgsl`).

use bytemuck::{Pod, Zeroable};

pub const MAX_PHOTOS: usize = 48;

/// One source photo: orientation, lens and exposure. Layout mirrors `Photo` in the shader.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub struct ComposePhoto {
    /// World → camera rotation rows; `.w` holds lens kind, focal length (px) and normalisation radius (px).
    pub r0: [f32; 4],
    pub r1: [f32; 4],
    pub r2: [f32; 4],
    /// Principal point (px) and image size (px).
    pub centre: [f32; 4],
    /// Distortion a, b, c and d0 = 1 − a − b − c.
    pub poly: [f32; 4],
    /// Largest valid normalised radius, largest angle from the optical axis (radians).
    pub limits: [f32; 4],
    /// Linear RGB gain; `.w` = 1 to draw the photo.
    pub gain: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Uniforms {
    w0: [f32; 4],
    w1: [f32; 4],
    w2: [f32; 4],
    rect: [f32; 4],
    crop: [f32; 4],
    photos: [ComposePhoto; MAX_PHOTOS],
}

/// Everything that changes per frame.
#[derive(Clone, Debug)]
pub struct ComposeFrame {
    /// Rotation applied to canvas directions before they are looked up in the photos (row-major).
    pub world: [[f32; 3]; 3],
    /// Viewport in physical pixels: origin x, y, width, height.
    pub rect: [f32; 4],
    /// Canvas window: yaw at the left edge, yaw span, pitch at the top edge, pitch span (radians).
    pub crop: [f32; 4],
    pub photos: Vec<ComposePhoto>,
}

struct Layers {
    _texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    count: usize,
}

pub struct Composer {
    pipeline: wgpu::RenderPipeline,
    bgl: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    uniform_buf: wgpu::Buffer,
    layers: Option<Layers>,
    encode_srgb: bool,
}

impl Composer {
    pub fn new(device: &wgpu::Device, target_format: wgpu::TextureFormat) -> Self {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("nodal compose"),
            source: wgpu::ShaderSource::Wgsl(include_str!("compose.wgsl").into()),
        });
        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("nodal compose bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: wgpu::BufferSize::new(std::mem::size_of::<Uniforms>() as u64),
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2Array,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("nodal compose"),
            bind_group_layouts: &[Some(&bgl)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("nodal compose"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_fullscreen"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_compose"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("nodal compose"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let uniform_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("nodal compose uniforms"),
            size: std::mem::size_of::<Uniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Composer { pipeline, bgl, sampler, uniform_buf, layers: None, encode_srgb: !target_format.is_srgb() }
    }

    /// Number of photos currently uploaded.
    pub fn count(&self) -> usize {
        self.layers.as_ref().map_or(0, |l| l.count)
    }

    pub fn clear(&mut self) {
        self.layers = None;
    }

    /// Uploads the photos, each already resampled to `width × height` RGBA8 (sRGB).
    pub fn set_layers(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, width: u32, height: u32, photos: &[&[u8]]) {
        let count = photos.len().min(MAX_PHOTOS);
        if count == 0 {
            self.layers = None;
            return;
        }
        // Never exactly 6 layers: wgpu's GL backend would treat that as a cube map.
        let layers = (count as u32).max(2) + (count == 6) as u32;
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("nodal compose photos"),
            size: wgpu::Extent3d { width, height, depth_or_array_layers: layers },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        for (i, data) in photos.iter().take(count).enumerate() {
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d { x: 0, y: 0, z: i as u32 },
                    aspect: wgpu::TextureAspect::All,
                },
                data,
                wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(4 * width), rows_per_image: Some(height) },
                wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            );
        }
        let view = texture
            .create_view(&wgpu::TextureViewDescriptor { dimension: Some(wgpu::TextureViewDimension::D2Array), ..Default::default() });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("nodal compose"),
            layout: &self.bgl,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: self.uniform_buf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&view) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(&self.sampler) },
            ],
        });
        self.layers = Some(Layers { _texture: texture, bind_group, count });
    }

    /// Writes this frame's parameters.
    pub fn write(&self, queue: &wgpu::Queue, frame: &ComposeFrame) {
        let mut u = Uniforms::zeroed();
        let n = frame.photos.len().min(self.count());
        u.w0 = [frame.world[0][0], frame.world[0][1], frame.world[0][2], n as f32];
        u.w1 = [frame.world[1][0], frame.world[1][1], frame.world[1][2], if self.encode_srgb { 1.0 } else { 0.0 }];
        u.w2 = [frame.world[2][0], frame.world[2][1], frame.world[2][2], 0.0];
        u.rect = frame.rect;
        u.crop = frame.crop;
        u.photos[..n].copy_from_slice(&frame.photos[..n]);
        queue.write_buffer(&self.uniform_buf, 0, bytemuck::bytes_of(&u));
    }

    /// Renders one frame into `target` (offscreen use).
    pub fn render_to(&self, device: &wgpu::Device, queue: &wgpu::Queue, target: &wgpu::TextureView, frame: &ComposeFrame) {
        self.write(queue, frame);
        let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("nodal compose") });
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("nodal compose"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT), store: wgpu::StoreOp::Store },
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

    pub fn paint(&self, rpass: &mut wgpu::RenderPass<'_>) {
        if let Some(layers) = &self.layers {
            rpass.set_pipeline(&self.pipeline);
            rpass.set_bind_group(0, &layers.bind_group, &[]);
            rpass.draw(0..3, 0..1);
        }
    }
}

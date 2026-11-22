//! Offscreen rendering without a window (CLI stills, thumbnails, tests).

use crate::fetch::ThreadFetcher;
use crate::{PanoEngine, Streamer};
use nodal_core::tour::Scene;
use nodal_core::{Projector, View};
use std::time::{Duration, Instant};

pub struct Headless {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub engine: PanoEngine,
    pub adapter: String,
    generation: u32,
}

const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

impl Headless {
    pub fn new() -> Result<Self, String> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::from_env().unwrap_or(wgpu::Backends::PRIMARY | wgpu::Backends::GL),
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: None,
            ..Default::default()
        }))
        .map_err(|e| format!("no GPU adapter: {e}"))?;
        let info = adapter.get_info();
        let limits = adapter.limits();
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("nodal headless"),
            required_limits: wgpu::Limits {
                max_texture_dimension_2d: limits.max_texture_dimension_2d.min(16384),
                ..wgpu::Limits::downlevel_defaults()
            },
            ..Default::default()
        }))
        .map_err(|e| format!("device: {e}"))?;
        let engine = PanoEngine::new(&device, FORMAT);
        Ok(Self { device, queue, engine, adapter: format!("{} ({:?})", info.name, info.backend), generation: 0 })
    }

    #[allow(clippy::too_many_arguments)]
    /// Renders one view of a scene, waiting (up to `timeout`) until every visible tile is loaded.
    pub fn render(
        &mut self,
        tour_base: &str,
        scene: &Scene,
        background: [u8; 3],
        view: &View,
        width: u32,
        height: u32,
        timeout: Duration,
    ) -> Result<image::RgbaImage, String> {
        self.generation += 1;
        self.engine.set_scene(&self.device, self.generation, scene.pyramid.clone(), background);
        let mut fetcher = ThreadFetcher::new(std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(2, 8), None);
        let mut streamer = Streamer::new(self.generation, tour_base.to_string(), scene.pyramid.clone());
        let proj = Projector::new(view, width as f32, height as f32);
        let start = Instant::now();
        loop {
            let st = streamer.step(&mut self.engine, &mut fetcher, &self.device, &self.queue, &proj, 0.0, 64);
            if st.is_complete() && st.pending == 0 {
                break;
            }
            if start.elapsed() > timeout {
                return Err(format!("timed out waiting for tiles ({} missing)", st.missing_on_screen));
            }
            if st.uploaded == 0 {
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        self.read_back(&proj, width, height)
    }

    /// Debug helper: reads one layer/mip of the base texture.
    pub fn read_base(&self, layer: u32, mip: u32) -> image::RgbaImage {
        let scene = self.engine.scene.as_ref().unwrap();
        let size = scene.base.size().width >> mip;
        let padded = (size * 4).div_ceil(256) * 256;
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: (padded * size) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut enc = self.device.create_command_encoder(&Default::default());
        enc.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &scene.base,
                mip_level: mip,
                origin: wgpu::Origin3d { x: 0, y: 0, z: layer },
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(padded), rows_per_image: Some(size) },
            },
            wgpu::Extent3d { width: size, height: size, depth_or_array_layers: 1 },
        );
        self.queue.submit([enc.finish()]);
        let slice = buffer.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| {});
        self.device.poll(wgpu::PollType::Wait { submission_index: None, timeout: None }).unwrap();
        let data = slice.get_mapped_range().unwrap();
        let mut out = vec![];
        for r in 0..size {
            let st = (r * padded) as usize;
            out.extend_from_slice(&data[st..st + (size * 4) as usize]);
        }
        image::RgbaImage::from_raw(size, size, out).unwrap()
    }

    fn read_back(&mut self, proj: &Projector, width: u32, height: u32) -> Result<image::RgbaImage, String> {
        let engine = &self.engine;
        let (device, queue) = (&self.device, &self.queue);
        self.draw_and_read(width, height, |view| engine.render_to(device, queue, view, proj, 0.0))
    }

    /// Renders into an offscreen target with `draw` and reads the pixels back.
    pub fn draw_and_read(&self, width: u32, height: u32, draw: impl FnOnce(&wgpu::TextureView)) -> Result<image::RgbaImage, String> {
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("nodal still"),
            size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&Default::default());
        draw(&view);
        let padded = (width * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("nodal readback"),
            size: (padded * height) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut enc = self.device.create_command_encoder(&Default::default());
        enc.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo { texture: &texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(padded), rows_per_image: Some(height) },
            },
            wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        );
        self.queue.submit([enc.finish()]);
        let slice = buffer.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        self.device
            .poll(wgpu::PollType::Wait { submission_index: None, timeout: Some(Duration::from_secs(60)) })
            .map_err(|e| format!("poll: {e}"))?;
        rx.recv().map_err(|e| e.to_string())?.map_err(|e| e.to_string())?;
        let data = slice.get_mapped_range().map_err(|e| format!("map: {e:?}"))?;
        let mut out = Vec::with_capacity((width * height * 4) as usize);
        for row in 0..height {
            let start = (row * padded) as usize;
            out.extend_from_slice(&data[start..start + (width * 4) as usize]);
        }
        drop(data);
        buffer.unmap();
        image::RgbaImage::from_raw(width, height, out).ok_or_else(|| "readback size".to_string())
    }
}

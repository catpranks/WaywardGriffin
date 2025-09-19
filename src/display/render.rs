use anyhow::{Context as _, Result, bail};
use raw_window_handle::{
    RawDisplayHandle, RawWindowHandle, WaylandDisplayHandle, WaylandWindowHandle,
};
use smithay_client_toolkit::reexports::client::protocol::wl_surface::WlSurface;
use smithay_client_toolkit::reexports::client::{Connection, Proxy as _};
use std::ptr::NonNull;
use std::time::Instant;
use tracing::info;
use wgpu::SurfaceTargetUnsafe;

use crate::GlobalState;
use crate::sizer::Sizer;

pub struct Renderer {
    _instance: wgpu::Instance,
    _adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    surface_config: wgpu::SurfaceConfiguration,
    frame_num: u64,
    egui_ctx: egui::Context,
    egui_renderer: egui_wgpu::Renderer,
    global_state: GlobalState,
    start_time: Instant,
}

impl Renderer {
    pub fn new(conn: &Connection, surface: &WlSurface, global_state: GlobalState) -> Result<Self> {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN,
            ..Default::default()
        });

        let raw_display_handle = RawDisplayHandle::Wayland(WaylandDisplayHandle::new(
            NonNull::new(conn.backend().display_ptr() as *mut _).unwrap(),
        ));
        let raw_window_handle = RawWindowHandle::Wayland(WaylandWindowHandle::new(
            NonNull::new(surface.id().as_ptr() as *mut _).unwrap(),
        ));

        let surface = unsafe {
            instance
                .create_surface_unsafe(SurfaceTargetUnsafe::RawHandle {
                    raw_display_handle,
                    raw_window_handle,
                })
                .unwrap()
        };

        for adapter in instance.enumerate_adapters(wgpu::Backends::VULKAN) {
            info!(
                "adapter {:?}, surface {}",
                adapter.get_info(),
                adapter.is_surface_supported(&surface)
            );
        }

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            compatible_surface: Some(&surface),
            ..Default::default()
        }))
        .context("Failed to find suitable adapter")?;
        info!("Using adapter {:?}", adapter.get_info());

        let (device, queue) = pollster::block_on(adapter.request_device(&Default::default()))?;

        let caps = surface.get_capabilities(&adapter);
        const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Bgra8Unorm;
        if !caps.formats.contains(&FORMAT) {
            bail!("Surface does not support Bgra8Unorm");
        }
        let surface_config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: FORMAT,
            view_formats: vec![FORMAT],
            alpha_mode: wgpu::CompositeAlphaMode::PreMultiplied,
            width: 400,
            height: 400,
            desired_maximum_frame_latency: 2,
            present_mode: wgpu::PresentMode::Mailbox,
        };

        surface.configure(&device, &surface_config);

        let egui_ctx = egui::Context::default();
        let egui_renderer = egui_wgpu::Renderer::new(&device, surface_config.format, None, 1, true);

        Ok(Self {
            _instance: instance,
            _adapter: adapter,
            device,
            queue,
            surface,
            surface_config,
            frame_num: 0,
            egui_ctx,
            egui_renderer,
            global_state,
            start_time: Instant::now(),
        })
    }

    pub fn resize(&mut self, sizer: &Sizer) {
        let (width, height) = sizer.window_size;
        let scale = sizer.scale120 as f64 / 120.0;
        self.surface_config.width = (width as f64 * scale) as u32;
        self.surface_config.height = (height as f64 * scale) as u32;
        self.surface.configure(&self.device, &self.surface_config);
    }

    pub fn render(&mut self) -> Result<(), wgpu::SurfaceError> {
        let surface_texture = self.surface.get_current_texture()?;
        let texture_view = surface_texture
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = self.device.create_command_encoder(&Default::default());

        let screen_descriptor = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [self.surface_config.width, self.surface_config.height],
            pixels_per_point: 1.0,
        };

        let raw_input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_max(
                egui::Pos2::ZERO,
                egui::pos2(
                    self.surface_config.width as f32,
                    self.surface_config.height as f32,
                ),
            )),
            ..Default::default()
        };

        let full_output = self.egui_ctx.run(raw_input, |ctx| {
            if self.global_state.load().confine {
                return;
            }
            let elapsed = self.start_time.elapsed().as_secs_f32();
            let sine_val = ((elapsed * 2.0).sin() + 1.0) / 2.0;

            let rect = ctx.screen_rect();
            let painter = ctx.layer_painter(egui::LayerId::background());

            let alpha = 5.0 + sine_val * 150.0;
            let color = egui::Color32::from_rgba_premultiplied(0, 200, 200, alpha as u8);
            let width = 15.0 + sine_val * 10.0;

            painter.rect_stroke(
                rect.shrink(width / 2.0),
                0.0,
                egui::Stroke::new(width, color),
                egui::StrokeKind::Middle,
            );
        });

        let tris = self
            .egui_ctx
            .tessellate(full_output.shapes, self.egui_ctx.pixels_per_point());

        for (id, image_delta) in &full_output.textures_delta.set {
            self.egui_renderer
                .update_texture(&self.device, &self.queue, *id, image_delta);
        }

        self.egui_renderer.update_buffers(
            &self.device,
            &self.queue,
            &mut encoder,
            &tris,
            &screen_descriptor,
        );

        {
            let rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("egui main render pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &texture_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });

            // forget_lifetime is required for some stupid wgpu reason. Leave it alone.
            self.egui_renderer
                .render(&mut rpass.forget_lifetime(), &tris, &screen_descriptor);
        }

        for tex_id in &full_output.textures_delta.free {
            self.egui_renderer.free_texture(tex_id);
        }

        self.queue.submit(Some(encoder.finish()));
        surface_texture.present();
        self.frame_num += 1;
        Ok(())
    }
}

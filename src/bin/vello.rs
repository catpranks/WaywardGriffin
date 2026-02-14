use anyhow::Result;
use clap::Parser;
use std::sync::Arc;
use std::time::Instant;
use vello::kurbo::{Affine, Rect};
use vello::peniko::{Color, Fill};
use vello::util::{RenderContext, RenderSurface};
use vello::wgpu;
use vello::{AaConfig, Renderer, RendererOptions, Scene};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::window::Window;

#[derive(Debug)]
enum RenderState {
    Active {
        surface: Box<RenderSurface<'static>>,
        valid_surface: bool,
        window: Arc<Window>,
    },
    Suspended(Option<Arc<Window>>),
}

struct App {
    context: RenderContext,
    renderers: Vec<Option<Renderer>>,
    state: RenderState,
    scene: Scene,
    start: Instant,
    frames_rendered: u32,
    max_frames: Option<u32>,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let RenderState::Suspended(cached_window) = &mut self.state else {
            return;
        };

        let window = cached_window.take().unwrap_or_else(|| {
            let attr = Window::default_attributes()
                .with_title("vello-minimal")
                .with_transparent(true);
            Arc::new(event_loop.create_window(attr).unwrap())
        });

        let size = window.inner_size();
        let mut surface = pollster::block_on(self.context.create_surface(
            window.clone(),
            size.width,
            size.height,
            wgpu::PresentMode::AutoVsync,
        ))
        .expect("failed to create surface");

        surface.config.alpha_mode = wgpu::CompositeAlphaMode::PreMultiplied;
        surface.surface.configure(
            &self.context.devices[surface.dev_id].device,
            &surface.config,
        );

        self.renderers
            .resize_with(self.context.devices.len(), || None);
        self.renderers[surface.dev_id].get_or_insert_with(|| {
            Renderer::new(
                &self.context.devices[surface.dev_id].device,
                RendererOptions::default(),
            )
            .expect("failed to create renderer")
        });

        self.state = RenderState::Active {
            surface: Box::new(surface),
            valid_surface: true,
            window,
        };
    }

    fn suspended(&mut self, _event_loop: &ActiveEventLoop) {
        if let RenderState::Active { window, .. } = &self.state {
            self.state = RenderState::Suspended(Some(window.clone()));
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: winit::window::WindowId,
        event: WindowEvent,
    ) {
        let (surface, valid_surface, window) = match &mut self.state {
            RenderState::Active {
                surface,
                valid_surface,
                window,
            } if window.id() == window_id => (surface, valid_surface, window.clone()),
            _ => return,
        };

        match event {
            WindowEvent::CloseRequested => event_loop.exit(),

            WindowEvent::Resized(size) => {
                if size.width != 0 && size.height != 0 {
                    self.context
                        .resize_surface(surface, size.width, size.height);
                    *valid_surface = true;
                } else {
                    *valid_surface = false;
                }
            }

            WindowEvent::RedrawRequested => {
                if !*valid_surface {
                    return;
                }

                let t = self.start.elapsed().as_secs_f64();
                let x = (t.sin() * 200.0) + 400.0;
                let y = (t.cos() * 150.0) + 300.0;

                self.scene.reset();

                self.scene.fill(
                    Fill::NonZero,
                    Affine::IDENTITY,
                    Color::new([1.0, 0.0, 0.0, 1.0]),
                    None,
                    &Rect::new(x, y, x + 60.0, y + 60.0),
                );

                let width = surface.config.width;
                let height = surface.config.height;
                let device_handle = &self.context.devices[surface.dev_id];

                self.renderers[surface.dev_id]
                    .as_mut()
                    .unwrap()
                    .render_to_texture(
                        &device_handle.device,
                        &device_handle.queue,
                        &self.scene,
                        &surface.target_view,
                        &vello::RenderParams {
                            base_color: Color::new([0.0, 0.0, 0.0, 0.0]),
                            width,
                            height,
                            antialiasing_method: AaConfig::Msaa16,
                        },
                    )
                    .expect("failed to render");

                let surface_texture = surface
                    .surface
                    .get_current_texture()
                    .expect("failed to get surface texture");

                let mut encoder =
                    device_handle
                        .device
                        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                            label: Some("Surface Blit"),
                        });
                surface.blitter.copy(
                    &device_handle.device,
                    &mut encoder,
                    &surface.target_view,
                    &surface_texture
                        .texture
                        .create_view(&wgpu::TextureViewDescriptor::default()),
                );
                device_handle.queue.submit([encoder.finish()]);
                surface_texture.present();

                device_handle.device.poll(wgpu::PollType::Poll).unwrap();

                self.frames_rendered += 1;
                if self.max_frames.is_some_and(|max| self.frames_rendered >= max) {
                    event_loop.exit();
                    return;
                }
                window.request_redraw();
            }

            _ => {}
        }
    }
}

#[derive(Parser)]
struct Args {
    /// Exit after rendering this many frames
    #[arg(long)]
    max_frames: Option<u32>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let mut app = App {
        context: RenderContext::new(),
        renderers: vec![],
        state: RenderState::Suspended(None),
        scene: Scene::new(),
        start: Instant::now(),
        frames_rendered: 0,
        max_frames: args.max_frames,
    };

    let event_loop = EventLoop::new()?;
    event_loop
        .run_app(&mut app)
        .expect("couldn't run event loop");
    Ok(())
}

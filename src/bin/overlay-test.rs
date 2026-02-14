use anyhow::{Context as _, Result};
use arc_swap::ArcSwap;
use std::process::{Child, Command};
use std::sync::Arc;
use tracing::info;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;
use vulkano::VulkanLibrary;
use vulkano::device::{
    Device, DeviceCreateInfo, DeviceExtensions, DeviceFeatures, QueueCreateInfo,
};
use vulkano::instance::{Instance, InstanceCreateInfo, InstanceExtensions};
use waygriff::GlobalStateInner;
use waygriff::overlay::{self, OverlayEnv};
use waygriff::plotter::Plotter;
use waygriff::sizer::Sizer;

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,waygriff=debug")),
        )
        .with(tracing_subscriber::fmt::layer().without_time().compact())
        .init();

    let library = VulkanLibrary::new().context("no Vulkan library")?;
    let instance = Instance::new(
        library,
        InstanceCreateInfo {
            enabled_extensions: InstanceExtensions {
                khr_external_memory_capabilities: true,
                ..InstanceExtensions::empty()
            },
            ..Default::default()
        },
    )?;
    let physical = instance
        .enumerate_physical_devices()?
        .next()
        .context("no GPU")?;

    let queue_family_index = physical
        .queue_family_properties()
        .iter()
        .position(|q| {
            q.queue_flags
                .intersects(vulkano::device::QueueFlags::GRAPHICS)
        })
        .map(|i| i as u32)
        .context("no graphics queue family")?;

    let (_device, _queues) = Device::new(
        physical.clone(),
        DeviceCreateInfo {
            enabled_extensions: DeviceExtensions {
                khr_external_memory: true,
                khr_external_memory_fd: true,
                ext_external_memory_dma_buf: true,
                ext_image_drm_format_modifier: true,
                khr_timeline_semaphore: true,
                ..DeviceExtensions::empty()
            },
            enabled_features: DeviceFeatures {
                timeline_semaphore: true,
                ..Default::default()
            },
            queue_create_infos: vec![QueueCreateInfo {
                queue_family_index,
                ..Default::default()
            }],
            ..Default::default()
        },
    )?;

    let global_state = Arc::new(ArcSwap::from_pointee(GlobalStateInner::default()));
    let sizer = Arc::new(ArcSwap::from_pointee(Sizer::default()));
    let plotter = Plotter::new(global_state, sizer);
    let ph = plotter.handle();

    let handle = overlay::spawn(OverlayEnv {
        physical_device: physical,
        ph,
    })?;

    info!(
        "Overlay compositor listening. Run: WAYLAND_DISPLAY={} <app>",
        handle.socket_name
    );

    let vello_bin = concat!(env!("CARGO_MANIFEST_DIR"), "/target/release/vello");
    info!(vello_bin, "spawning vello client");
    let child = Command::new(vello_bin)
        .env("WAYLAND_DISPLAY", &handle.socket_name)
        .env("WAYLAND_DEBUG", "1")
        .spawn()
        .context("failed to spawn vello")?;
    let guard = ChildGuard(child);

    let ph2 = plotter.handle();
    std::thread::Builder::new()
        .name("overlay-poll".into())
        .spawn(move || {
            let _guard = guard;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
            let mut got_frame = false;
            while std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(1));
                if let Some(frame) = handle.slot.lock().unwrap().take() {
                    use smithay::backend::allocator::Buffer as _;
                    let fmt = frame.dmabuf.format();
                    info!(
                        w = frame.size.0,
                        h = frame.size.1,
                        format = ?fmt.code,
                        modifier = format!("{:#x}", u64::from(fmt.modifier)),
                        "overlay frame received"
                    );
                    got_frame = true;
                    break;
                }
            }
            if !got_frame {
                info!("no frame received within 1s");
            }
            ph2.fatal(Ok(()));
        })
        .unwrap();

    plotter.run(None)
}

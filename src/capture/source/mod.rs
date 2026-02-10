mod nvfbc;
pub mod wayland;

use super::CaptureOpts;
use super::SwapchainRenderer;
use super::input::InputBridge;
use crate::GlobalState;
use crate::capture::plotter::PlotterHandle;
use crate::display::DisplayCtx;
use anyhow::{Context as _, Result};
use clap::ValueEnum;
use smithay_client_toolkit::reexports::client::{Connection, Proxy as _};
use std::sync::Arc;
use vulkano::VulkanLibrary;
use vulkano::device::{
    Device, DeviceCreateInfo, DeviceExtensions, DeviceFeatures, QueueCreateInfo, QueueFlags,
};
use vulkano::instance::{Instance, InstanceCreateInfo, InstanceExtensions};
use vulkano::memory::allocator::StandardMemoryAllocator;
use vulkano::swapchain::Surface;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum BackendType {
    Nvfbc,
    Screencopy,
    Imagecopy,
    Kms,
}

pub enum DeviceId {
    Uuid([u8; 16]),
    DevMajorMinor(u64, u64),
}

pub struct CaptureEnv {
    pub renderer: SwapchainRenderer,
    pub ph: PlotterHandle,
    pub global_state: GlobalState,
    pub device: Arc<Device>,
    pub allocator: Arc<StandardMemoryAllocator>,
    pub backend: BackendType,
}

pub struct SpawnResult {
    pub injector: Box<dyn InputBridge>,
    pub wake: Box<dyn Fn() + Send>,
}

pub trait CaptureBackend: Send + 'static {
    fn device_id(&self) -> DeviceId;

    fn spawn(self: Box<Self>, env: CaptureEnv) -> Result<SpawnResult>;
}

pub fn setup_and_spawn(
    opts: &CaptureOpts,
    dc: DisplayCtx,
    conn: &Connection,
) -> Result<SpawnResult> {
    let backend: Box<dyn CaptureBackend> = match opts.backend {
        BackendType::Nvfbc => Box::new(nvfbc::Backend::new(&opts.display)?),
        BackendType::Screencopy | BackendType::Imagecopy => {
            Box::new(wayland::Backend::new(&opts.display)?)
        }
        BackendType::Kms => todo!("kms backend"),
    };

    let device_id = backend.device_id();

    let library = VulkanLibrary::new().context("no Vulkan library")?;
    let instance = Instance::new(
        library,
        InstanceCreateInfo {
            enabled_extensions: InstanceExtensions {
                khr_wayland_surface: true,
                khr_external_memory_capabilities: true,
                ..InstanceExtensions::empty()
            },
            ..Default::default()
        },
    )?;

    let physical_device = match device_id {
        DeviceId::Uuid(uuid) => instance
            .enumerate_physical_devices()?
            .find(|p| p.properties().device_uuid == Some(uuid))
            .context("no physical device with matching UUID")?,
        DeviceId::DevMajorMinor(major, minor) => instance
            .enumerate_physical_devices()?
            .find(|p| {
                let props = p.properties();
                let major = major as i64;
                let minor = minor as i64;
                (props.primary_major == Some(major) && props.primary_minor == Some(minor))
                    || (props.render_major == Some(major) && props.render_minor == Some(minor))
            })
            .context("no physical device with matching major/minor")?,
    };

    let surface = unsafe {
        Surface::from_wayland(
            instance,
            conn.backend().display_ptr() as _,
            dc.surface.id().as_ptr() as _,
            None,
        )?
    };

    let device_extensions = DeviceExtensions {
        khr_swapchain: true,
        khr_external_memory: true,
        khr_external_memory_fd: true,
        ext_external_memory_dma_buf: true,
        khr_external_semaphore_fd: true,
        khr_timeline_semaphore: true,
        ext_image_drm_format_modifier: true,
        ..DeviceExtensions::empty()
    };

    let queue_family_index = physical_device
        .queue_family_properties()
        .iter()
        .enumerate()
        .position(|(i, q)| {
            q.queue_flags.intersects(QueueFlags::GRAPHICS)
                && physical_device
                    .surface_support(i as u32, &surface)
                    .unwrap_or(false)
        })
        .map(|i| i as u32)
        .context("No graphics queue family found on the device")?;

    let (device, mut queues) = Device::new(
        physical_device,
        DeviceCreateInfo {
            enabled_extensions: device_extensions,
            queue_create_infos: vec![QueueCreateInfo {
                queue_family_index,
                ..Default::default()
            }],
            enabled_features: DeviceFeatures {
                timeline_semaphore: true,
                ..Default::default()
            },
            ..Default::default()
        },
    )?;

    let queue = queues.next().unwrap();
    let allocator = Arc::new(StandardMemoryAllocator::new_default(device.clone()));

    let ph = dc.ph.clone();
    let global_state = dc.global_state.clone();

    let renderer = SwapchainRenderer::new(dc, device.clone(), queue, surface)?;

    let env = CaptureEnv {
        renderer,
        ph,
        global_state,
        device,
        allocator,
        backend: opts.backend,
    };
    backend.spawn(env)
}

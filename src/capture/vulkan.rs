use super::source::{CaptureBackendBuilder, DeviceId};
use anyhow::{Context as _, Result};
use std::sync::Arc;
use vulkano::VulkanLibrary;
use vulkano::device::physical::PhysicalDevice;
use vulkano::instance::{Instance, InstanceCreateInfo, InstanceExtensions};

pub fn create_instance_and_select_device(
    backend_builder: &dyn CaptureBackendBuilder,
) -> Result<(Arc<Instance>, Arc<PhysicalDevice>)> {
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

    let physical_device = match backend_builder.device_id() {
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

    Ok((instance, physical_device))
}

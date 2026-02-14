use anyhow::{Context as _, Result, anyhow, bail};
use drm_fourcc::DrmFourcc;
use smithay_client_toolkit::reexports::client::Connection;
use smithay_client_toolkit::reexports::client::protocol::wl_buffer::WlBuffer;
use std::mem::MaybeUninit;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use vulkano::VulkanObject as _;
use vulkano::device::Device;
use vulkano::format::Format;
use vulkano::image::sys::RawImage;
use vulkano::image::{ImageCreateInfo, ImageTiling, ImageUsage};
use vulkano::memory::ExternalMemoryHandleTypes;

pub fn clock_monotonic_ns() -> u64 {
    let ts = nix::time::clock_gettime(nix::time::ClockId::CLOCK_MONOTONIC).unwrap();
    ts.tv_sec() as u64 * 1_000_000_000 + ts.tv_nsec() as u64
}

pub fn compose_timestamp(tv_sec_hi: u32, tv_sec_lo: u32, tv_nsec: u32) -> u64 {
    ((tv_sec_hi as u64) << 32 | tv_sec_lo as u64) * 1_000_000_000 + tv_nsec as u64
}

pub fn wayland_connect(display: &str) -> Result<Connection> {
    let stream = UnixStream::connect(display)
        .with_context(|| format!("Failed to connect to Wayland socket: {display}"))?;
    Connection::from_socket(stream).context("Failed to create Wayland connection from socket")
}

pub struct OwningWlBuffer(pub WlBuffer);

impl std::ops::Deref for OwningWlBuffer {
    type Target = WlBuffer;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for OwningWlBuffer {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Drop for OwningWlBuffer {
    fn drop(&mut self) {
        self.0.destroy();
    }
}

/// Create a Vulkan image with DRM format modifier tiling.
///
/// Workaround for vulkano 0.35.2 not chaining
/// VkImageDrmFormatModifierListCreateInfoEXT into pNext.
pub fn create_drm_modifier_image(
    device: Arc<Device>,
    format: Format,
    width: u32,
    height: u32,
    usage: ImageUsage,
    modifiers: Vec<u64>,
) -> Result<RawImage> {
    let handle = {
        let mut modifier_list = ash::vk::ImageDrmFormatModifierListCreateInfoEXT::default()
            .drm_format_modifiers(&modifiers);
        let mut external_mem = ash::vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(ash::vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let create_info_vk = ash::vk::ImageCreateInfo::default()
            .image_type(ash::vk::ImageType::TYPE_2D)
            .format(format.into())
            .extent(ash::vk::Extent3D {
                width,
                height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(ash::vk::SampleCountFlags::TYPE_1)
            .tiling(ash::vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(usage.into())
            .sharing_mode(ash::vk::SharingMode::EXCLUSIVE)
            .initial_layout(ash::vk::ImageLayout::UNDEFINED)
            .push_next(&mut modifier_list)
            .push_next(&mut external_mem);
        let mut output = MaybeUninit::uninit();
        unsafe {
            (device.fns().v1_0.create_image)(
                device.handle(),
                &create_info_vk,
                std::ptr::null(),
                output.as_mut_ptr(),
            )
        }
        .result()
        .map_err(|e| anyhow!("vkCreateImage: {e:?}"))?;
        unsafe { output.assume_init() }
    };
    unsafe {
        RawImage::from_handle(
            device,
            handle,
            ImageCreateInfo {
                format,
                extent: [width, height, 1],
                usage,
                external_memory_handle_types: ExternalMemoryHandleTypes::DMA_BUF,
                tiling: ImageTiling::DrmFormatModifier,
                drm_format_modifiers: modifiers,
                ..Default::default()
            },
        )
    }
    .context("RawImage::from_handle")
}

pub fn fourcc_to_vk_format(fourcc: DrmFourcc) -> Result<Format> {
    match fourcc {
        DrmFourcc::Argb8888 | DrmFourcc::Xrgb8888 => Ok(Format::B8G8R8A8_SRGB),
        DrmFourcc::Abgr8888 | DrmFourcc::Xbgr8888 => Ok(Format::R8G8B8A8_SRGB),
        other => bail!("unsupported fourcc: {other:?}"),
    }
}

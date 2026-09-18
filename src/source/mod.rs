mod nvfbc;
mod wayland;

use crate::GlobalState;
use crate::bridge::InputBridge;
use crate::monitor::FrameInfo;
use crate::monitor::MonitorHandle;
use crate::sizer::SharedSizer;
use anyhow::Result;
use clap::{Args, ValueEnum};
use std::any::Any;
use std::sync::Arc;
use std::sync::mpsc;
use vulkano::buffer::Subbuffer;
use vulkano::device::{Device, Queue};
use vulkano::image::Image;
use vulkano::memory::allocator::StandardMemoryAllocator;

#[derive(Debug, Clone, Args)]
pub struct CaptureOpts {
    /// The display to capture
    #[arg(long)]
    pub display: String,

    /// Capture backend to use
    #[arg(long, value_enum)]
    pub backend: BackendType,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum BackendType {
    Nvfbc,
    Screencopy,
    Imagecopy,
}

pub enum DeviceId {
    RadvIntegrated,
    DevMajorMinor(u64, u64),
}

pub struct CaptureEnv {
    pub render_tx: mpsc::Sender<crate::render::RenderMsg>,
    pub mon: MonitorHandle,
    pub global_state: GlobalState,
    pub device: Arc<Device>,
    pub queue: Arc<Queue>,
    pub allocator: Arc<StandardMemoryAllocator>,
}

pub trait CaptureBackendBuilder: 'static {
    fn device_id(&self) -> Result<DeviceId>;
    fn build(
        self: Box<Self>,
        env: CaptureEnv,
        sizer: SharedSizer,
    ) -> Result<(Box<dyn CaptureBackend>, Box<dyn InputBridge>)>;
}

pub trait CaptureBackend: Send + 'static {
    fn frame_driven(&self) -> bool {
        false
    }
    fn capture(&mut self) -> Result<Option<CapturedFrame>>;
}

/// Transfer view of the linear host image; its CapturedFrame owns the backing.
pub struct HostBuffer {
    pub bytes: Subbuffer<[u8]>,
    /// Image row pitch in texels, for VkBufferImageCopy.
    pub row_length: u32,
}

pub struct CapturedFrame {
    pub host_buffer: Option<HostBuffer>,
    pub image: Arc<Image>,
    pub backend_data: Option<Box<dyn Any + Send + Sync>>,
    pub info: Option<FrameInfo>,
    pub reclaim_tx: mpsc::Sender<ReclaimedBuffer>,
}

impl Drop for CapturedFrame {
    fn drop(&mut self) {
        let _ = self.reclaim_tx.send(ReclaimedBuffer {
            image: self.image.clone(),
            backend_data: self.backend_data.take().unwrap(),
        });
    }
}

pub struct ReclaimedBuffer {
    pub image: Arc<Image>,
    pub backend_data: Box<dyn Any + Send + Sync>,
}

pub fn create_backend_builder(opts: &CaptureOpts) -> Result<Box<dyn CaptureBackendBuilder>> {
    let b = match opts.backend {
        BackendType::Nvfbc => {
            Box::new(nvfbc::Builder::new(&opts.display)?) as Box<dyn CaptureBackendBuilder>
        }
        BackendType::Screencopy | BackendType::Imagecopy => {
            Box::new(wayland::Builder::new(&opts.display, opts.backend)?)
        }
    };
    Ok(b)
}

pub mod nvfbc;
pub mod wlr_screencopy;

use super::CaptureOpts;
use super::input::InputBridge;
use super::plotter::FrameInfo;
use anyhow::Result;
use clap::ValueEnum;
use std::any::Any;
use std::sync::Arc;
use vulkano::device::Device;
use vulkano::image::Image;
use vulkano::memory::allocator::StandardMemoryAllocator;
use vulkano::sync::fence::Fence;

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum BackendType {
    Nvfbc,
    WlrScreencopy,
    ExtImageCopy,
    Kms,
}

pub fn create_backend_builder(opts: &CaptureOpts) -> Result<Box<dyn CaptureBackendBuilder>> {
    match opts.backend {
        BackendType::Nvfbc => Ok(Box::new(nvfbc::Builder::new()?)),
        BackendType::WlrScreencopy => Ok(Box::new(wlr_screencopy::Builder::new(&opts.display)?)),
        BackendType::ExtImageCopy => todo!("ext-image-copy backend"),
        BackendType::Kms => todo!("kms backend"),
    }
}

pub struct CapturedFrame {
    pub image: Arc<Image>,
    pub info: FrameInfo,
    pub handle: Box<dyn Any + Send>,
}

pub enum DeviceId {
    Uuid([u8; 16]),
    DevMajorMinor(u64, u64),
}

pub trait CaptureBackendBuilder: Send {
    fn device_id(&self) -> DeviceId;

    fn build(
        self: Box<Self>,
        device: Arc<Device>,
        allocator: Arc<StandardMemoryAllocator>,
        display: &str,
    ) -> Result<(Box<dyn CaptureBackend>, Box<dyn InputBridge>)>;
}

pub trait CaptureBackend: Send {
    fn capture(&mut self) -> Result<CapturedFrame>;
    fn release(&mut self, frame: CapturedFrame, fence: Option<Arc<Fence>>);
    fn idle(&mut self) -> Result<()> {
        Ok(())
    }
}

pub mod nvfbc;
pub mod xinput;

use super::CaptureOpts;
use crate::capture::plotter::{FrameInfo, PlotterHandle};
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
        BackendType::WlrScreencopy => todo!("wlr-screencopy backend"),
        BackendType::ExtImageCopy => todo!("ext-image-copy backend"),
        BackendType::Kms => todo!("kms backend"),
    }
}

pub struct CapturedFrame {
    pub image: Arc<Image>,
    pub info: FrameInfo,
    pub(crate) handle: Box<dyn Any + Send>,
}

pub trait CaptureBackendBuilder: Send {
    fn device_uuid(&self) -> Option<[u8; 16]>;

    fn build(
        self: Box<Self>,
        device: Arc<Device>,
        allocator: Arc<StandardMemoryAllocator>,
        ph: PlotterHandle,
    ) -> Result<(Box<dyn CaptureBackend>, Box<dyn InputInjector>)>;
}

pub trait CaptureBackend: Send {
    fn capture(&mut self) -> Result<Option<CapturedFrame>>;
    fn release(&mut self, frame: CapturedFrame, fence: Option<Arc<Fence>>);
}

pub trait InputInjector: Send {
    fn mouse_delta(&mut self, x: f64, y: f64) -> Result<()>;
    fn mouse_absolute(&mut self, x: i32, y: i32) -> Result<()>;
    fn mouse_press(&mut self, button: u32) -> Result<()>;
    fn mouse_release(&mut self, button: u32) -> Result<()>;
    fn key_press(&mut self, keycode: u32) -> Result<()>;
    fn key_release(&mut self, keycode: u32) -> Result<()>;
    fn scroll(&mut self, h: i32, v: i32) -> Result<()>;
}

pub mod nvfbc;

use crate::capture::plotter::{FrameInfo, PlotterHandle};
use anyhow::Result;
use std::any::Any;
use std::sync::Arc;
use vulkano::device::Device;
use vulkano::image::Image;
use vulkano::memory::allocator::StandardMemoryAllocator;
use vulkano::sync::fence::Fence;

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
    ) -> Result<Box<dyn CaptureBackend>>;
}

pub trait CaptureBackend: Send {
    fn capture(&mut self) -> Result<Option<CapturedFrame>>;
    fn release(&mut self, frame: CapturedFrame, fence: Option<Arc<Fence>>);
}

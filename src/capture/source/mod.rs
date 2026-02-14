mod nvfbc;
mod wayland;

use super::CaptureOpts;
use super::input::InputBridge;
use crate::GlobalState;
use crate::plotter::FrameInfo;
use crate::plotter::PlotterHandle;
use crate::sizer::SharedSizer;
use anyhow::Result;
use clap::ValueEnum;
use std::any::Any;
use std::sync::Arc;
use std::sync::mpsc;
use vulkano::device::Device;
use vulkano::image::Image;
use vulkano::memory::allocator::StandardMemoryAllocator;

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
    pub ph: PlotterHandle,
    pub global_state: GlobalState,
    pub sizer: SharedSizer,
    pub device: Arc<Device>,
    pub allocator: Arc<StandardMemoryAllocator>,
}

pub trait CaptureBackendBuilder: 'static {
    fn device_id(&self) -> Result<DeviceId>;
    fn build(
        self: Box<Self>,
        env: CaptureEnv,
    ) -> Result<(Box<dyn CaptureBackend>, Box<dyn InputBridge>)>;
}

pub trait CaptureBackend: 'static {
    fn capture(&mut self) -> Result<Option<CapturedFrame>>;
}

pub struct CapturedFrame {
    pub image: Arc<Image>,
    pub backend_data: Option<Box<dyn Any + Send>>,
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
    pub backend_data: Box<dyn Any + Send>,
}

pub fn create_backend_builder(opts: &CaptureOpts) -> Result<Box<dyn CaptureBackendBuilder>> {
    let b = match opts.backend {
        BackendType::Nvfbc => {
            Box::new(nvfbc::Builder::new(&opts.display)?) as Box<dyn CaptureBackendBuilder>
        }
        BackendType::Screencopy | BackendType::Imagecopy => {
            Box::new(wayland::Builder::new(&opts.display, opts.backend)?)
        }
        BackendType::Kms => todo!("kms backend"),
    };
    Ok(b)
}

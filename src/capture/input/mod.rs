pub mod dummy;
pub mod xinput;

use anyhow::Result;

pub trait InputInjector: Send {
    fn mouse_delta(&mut self, x: f64, y: f64) -> Result<()>;
    fn mouse_absolute(&mut self, x: i32, y: i32) -> Result<()>;
    fn mouse_press(&mut self, button: u32) -> Result<()>;
    fn mouse_release(&mut self, button: u32) -> Result<()>;
    fn key_press(&mut self, keycode: u32) -> Result<()>;
    fn key_release(&mut self, keycode: u32) -> Result<()>;
    fn scroll(&mut self, h: i32, v: i32) -> Result<()>;
}

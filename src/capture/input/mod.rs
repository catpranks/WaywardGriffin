pub mod dummy;
pub mod wayland;
pub mod xinput;

use anyhow::Result;

pub trait InputBridge: Send {
    fn mouse_delta(&mut self, x: f64, y: f64) -> Result<()>;
    fn mouse_absolute(&mut self, x: u32, y: u32) -> Result<()>;
    fn mouse_press(&mut self, button: u32) -> Result<()>;
    fn mouse_release(&mut self, button: u32) -> Result<()>;
    fn key_press(&mut self, keycode: u32) -> Result<()>;
    fn key_release(&mut self, keycode: u32) -> Result<()>;
    fn scroll(&mut self, h_abs: f64, v_abs: f64, h120: i32, v120: i32) -> Result<()>;
    fn scroll_stop(&mut self, _horizontal: bool, _vertical: bool) -> Result<()> {
        Ok(())
    }

    fn get_primary(&mut self) -> Option<String> {
        None
    }
    fn set_primary(&mut self, _contents: String) {}
    fn get_clipboard(&mut self) -> Option<String> {
        None
    }
    fn set_clipboard(&mut self, _contents: String) {}
}

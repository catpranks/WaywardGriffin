use super::InputBridge;
use anyhow::Result;

#[allow(dead_code)]
pub struct DummyInput;

#[allow(dead_code)]
impl DummyInput {
    pub fn new() -> Self {
        Self
    }
}

impl InputBridge for DummyInput {
    fn mouse_delta(&mut self, _x: f64, _y: f64) -> Result<()> {
        Ok(())
    }

    fn mouse_absolute(&mut self, _x: u32, _y: u32) -> Result<()> {
        Ok(())
    }

    fn mouse_press(&mut self, _button: u32) -> Result<()> {
        Ok(())
    }

    fn mouse_release(&mut self, _button: u32) -> Result<()> {
        Ok(())
    }

    fn key_press(&mut self, _keycode: u32) -> Result<()> {
        Ok(())
    }

    fn key_release(&mut self, _keycode: u32) -> Result<()> {
        Ok(())
    }

    fn scroll(&mut self, _h_abs: f64, _v_abs: f64, _h120: i32, _v120: i32) -> Result<()> {
        Ok(())
    }
}

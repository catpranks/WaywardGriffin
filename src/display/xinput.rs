use anyhow::{Context as _, Result};
use std::collections::HashMap;
use x11rb::connection::Connection as _;
use x11rb::protocol::xproto::{self, ConnectionExt as _};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;
use xkeysym::Keysym;

pub struct XInput {
    conn: RustConnection,
    root: xproto::Window,
    keysym_to_keycode_map: HashMap<u32, u8>,
    scroll_h_accumulator: i32,
    scroll_v_accumulator: i32,
    mouse_x_accumulator: f64,
    mouse_y_accumulator: f64,
}

impl XInput {
    pub fn new() -> Result<Self> {
        let (conn, screen_num) =
            RustConnection::connect(None).context("Failed to connect to X server")?;
        conn.xtest_get_version(2, 1)?.reply()?;

        let setup = conn.setup();
        let root = setup.roots[screen_num].root;
        let min_keycode = setup.min_keycode;
        let mapping = conn
            .get_keyboard_mapping(min_keycode, setup.max_keycode - min_keycode + 1)?
            .reply()?;

        let mut keysym_to_keycode_map = HashMap::new();
        for keycode_offset in
            0..((mapping.keysyms.len() / mapping.keysyms_per_keycode as usize) as u8)
        {
            let start = keycode_offset as usize * mapping.keysyms_per_keycode as usize;
            let end = start + mapping.keysyms_per_keycode as usize;
            if let Some(keysym_slice) = mapping.keysyms.get(start..end) {
                // Map all available keysyms for this keycode (unshifted, shifted, etc.).
                // This allows lookups for uppercase letters and symbols like '$'.
                for &keysym in keysym_slice.iter() {
                    if keysym != 0 {
                        // The same keysym can be mapped to multiple keycodes (e.g., left/right variants).
                        // Prefer the first one encountered.
                        keysym_to_keycode_map
                            .entry(keysym)
                            .or_insert(min_keycode + keycode_offset);
                    }
                }
            }
        }

        Ok(Self {
            conn,
            root,
            keysym_to_keycode_map,
            scroll_h_accumulator: 0,
            scroll_v_accumulator: 0,
            mouse_x_accumulator: 0.0,
            mouse_y_accumulator: 0.0,
        })
    }

    fn evdev_to_x11_button(&self, evdev_code: u32) -> Option<u8> {
        match evdev_code {
            0x110 => Some(1), // BTN_LEFT
            0x111 => Some(3), // BTN_RIGHT
            0x112 => Some(2), // BTN_MIDDLE
            0x113 => Some(8), // BTN_SIDE
            0x114 => Some(9), // BTN_EXTRA
            _ => None,
        }
    }

    pub fn mouse_delta(&mut self, x: f64, y: f64) -> Result<()> {
        self.mouse_x_accumulator += x;
        self.mouse_y_accumulator += y;
        let x_send = self.mouse_x_accumulator.trunc() as i16;
        let y_send = self.mouse_y_accumulator.trunc() as i16;
        if x_send != 0 || y_send != 0 {
            self.conn.xtest_fake_input(6, 1, 0, 0, x_send, y_send, 0)?;
            self.conn.flush()?;
            self.mouse_x_accumulator -= f64::from(x_send);
            self.mouse_y_accumulator -= f64::from(y_send);
        }
        Ok(())
    }

    pub fn mouse_absolute(&mut self, x: i32, y: i32) -> Result<()> {
        self.conn
            .xtest_fake_input(6, 0, 0, self.root, x as i16, y as i16, 0)?;
        self.conn.flush()?;
        Ok(())
    }

    pub fn mouse_press(&mut self, button: u32) -> Result<()> {
        if let Some(x11_button) = self.evdev_to_x11_button(button) {
            self.conn.xtest_fake_input(4, x11_button, 0, 0, 0, 0, 0)?;
            self.conn.flush()?;
        }
        Ok(())
    }

    pub fn mouse_release(&mut self, button: u32) -> Result<()> {
        if let Some(x11_button) = self.evdev_to_x11_button(button) {
            self.conn.xtest_fake_input(5, x11_button, 0, 0, 0, 0, 0)?;
            self.conn.flush()?;
        }
        Ok(())
    }

    pub fn press(&mut self, key: Keysym) -> Result<()> {
        if let Some(&keycode) = self.keysym_to_keycode_map.get(&key.into()) {
            self.conn.xtest_fake_input(2, keycode, 0, 0, 0, 0, 0)?;
            self.conn.flush()?;
        }
        Ok(())
    }

    pub fn release(&mut self, key: Keysym) -> Result<()> {
        if let Some(&keycode) = self.keysym_to_keycode_map.get(&key.into()) {
            self.conn.xtest_fake_input(3, keycode, 0, 0, 0, 0, 0)?;
            self.conn.flush()?;
        }
        Ok(())
    }

    pub fn scroll(&mut self, h: i32, v: i32) -> Result<()> {
        let h_acc = &mut self.scroll_h_accumulator;
        *h_acc += h;
        let v_acc = &mut self.scroll_v_accumulator;
        *v_acc += v;
        while h_acc.abs() >= 120 {
            let button = if *h_acc > 0 { 7 } else { 6 }; // right/left
            self.conn.xtest_fake_input(4, button, 0, 0, 0, 0, 0)?;
            self.conn.xtest_fake_input(5, button, 0, 0, 0, 0, 0)?;
            *h_acc += if *h_acc > 0 { -120 } else { 120 };
        }
        while v_acc.abs() >= 120 {
            let button = if *v_acc > 0 { 5 } else { 4 }; // down/up
            self.conn.xtest_fake_input(4, button, 0, 0, 0, 0, 0)?;
            self.conn.xtest_fake_input(5, button, 0, 0, 0, 0, 0)?;
            *v_acc += if *v_acc > 0 { -120 } else { 120 };
        }

        self.conn.flush()?;
        Ok(())
    }
}

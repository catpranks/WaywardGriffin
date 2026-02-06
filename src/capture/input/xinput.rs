use super::InputBridge;
use anyhow::{Context as _, Result};
use copypasta::x11_clipboard::{
    Clipboard as X11Clipboard, Primary as X11Primary, X11ClipboardContext,
};
use copypasta::ClipboardProvider;
use x11rb::connection::Connection as _;
use x11rb::protocol::xproto;
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

pub struct XInput {
    conn: RustConnection,
    root: xproto::Window,
    scroll_h_accumulator: i32,
    scroll_v_accumulator: i32,
    mouse_x_accumulator: f64,
    mouse_y_accumulator: f64,
    primary: X11ClipboardContext<X11Primary>,
    clipboard: X11ClipboardContext<X11Clipboard>,
}

impl XInput {
    pub fn new(display: &str) -> Result<Self> {
        let (conn, screen_num) =
            RustConnection::connect(Some(display)).context("Failed to connect to X server")?;
        conn.xtest_get_version(2, 1)?.reply()?;

        let setup = conn.setup();
        let root = setup.roots[screen_num].root;

        let primary = X11ClipboardContext::<X11Primary>::new()
            .map_err(|e| anyhow::anyhow!("Failed to create X11 primary clipboard: {e}"))?;
        let clipboard = X11ClipboardContext::<X11Clipboard>::new()
            .map_err(|e| anyhow::anyhow!("Failed to create X11 clipboard: {e}"))?;

        Ok(Self {
            conn,
            root,
            scroll_h_accumulator: 0,
            scroll_v_accumulator: 0,
            mouse_x_accumulator: 0.0,
            mouse_y_accumulator: 0.0,
            primary,
            clipboard,
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
}

impl InputBridge for XInput {
    fn mouse_delta(&mut self, x: f64, y: f64) -> Result<()> {
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

    fn mouse_absolute(&mut self, x: i32, y: i32) -> Result<()> {
        self.conn
            .xtest_fake_input(6, 0, 0, self.root, x as i16, y as i16, 0)?;
        self.conn.flush()?;
        Ok(())
    }

    fn mouse_press(&mut self, button: u32) -> Result<()> {
        if let Some(x11_button) = self.evdev_to_x11_button(button) {
            self.conn.xtest_fake_input(4, x11_button, 0, 0, 0, 0, 0)?;
            self.conn.flush()?;
        }
        Ok(())
    }

    fn mouse_release(&mut self, button: u32) -> Result<()> {
        if let Some(x11_button) = self.evdev_to_x11_button(button) {
            self.conn.xtest_fake_input(5, x11_button, 0, 0, 0, 0, 0)?;
            self.conn.flush()?;
        }
        Ok(())
    }

    fn key_press(&mut self, keycode: u32) -> Result<()> {
        let Ok(x11_keycode) = u8::try_from(keycode + 8) else {
            return Ok(());
        };
        self.conn.xtest_fake_input(2, x11_keycode, 0, 0, 0, 0, 0)?;
        self.conn.flush()?;
        Ok(())
    }

    fn key_release(&mut self, keycode: u32) -> Result<()> {
        let Ok(x11_keycode) = u8::try_from(keycode + 8) else {
            return Ok(());
        };
        self.conn.xtest_fake_input(3, x11_keycode, 0, 0, 0, 0, 0)?;
        self.conn.flush()?;
        Ok(())
    }

    fn scroll(&mut self, h: i32, v: i32) -> Result<()> {
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

    fn get_primary(&mut self) -> Option<String> {
        self.primary.get_contents().ok()
    }

    fn set_primary(&mut self, contents: String) {
        let _ = self.primary.set_contents(contents);
    }

    fn get_clipboard(&mut self) -> Option<String> {
        self.clipboard.get_contents().ok()
    }

    fn set_clipboard(&mut self, contents: String) {
        let _ = self.clipboard.set_contents(contents);
    }
}

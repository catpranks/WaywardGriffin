use anyhow::{Context as _, Result};
use smithay_client_toolkit::reexports::client::Connection;
use smithay_client_toolkit::reexports::client::protocol::wl_buffer::WlBuffer;
use std::os::unix::net::UnixStream;

pub fn clock_monotonic_ns() -> u64 {
    let ts = nix::time::clock_gettime(nix::time::ClockId::CLOCK_MONOTONIC).unwrap();
    ts.tv_sec() as u64 * 1_000_000_000 + ts.tv_nsec() as u64
}

pub fn compose_timestamp(tv_sec_hi: u32, tv_sec_lo: u32, tv_nsec: u32) -> u64 {
    ((tv_sec_hi as u64) << 32 | tv_sec_lo as u64) * 1_000_000_000 + tv_nsec as u64
}

pub fn wayland_connect(display: &str) -> Result<Connection> {
    let stream = UnixStream::connect(display)
        .with_context(|| format!("Failed to connect to Wayland socket: {display}"))?;
    Connection::from_socket(stream).context("Failed to create Wayland connection from socket")
}

pub struct OwningWlBuffer(pub WlBuffer);

impl std::ops::Deref for OwningWlBuffer {
    type Target = WlBuffer;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for OwningWlBuffer {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Drop for OwningWlBuffer {
    fn drop(&mut self) {
        self.0.destroy();
    }
}

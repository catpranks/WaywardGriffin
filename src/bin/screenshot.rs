use anyhow::{Context, Result};
use image::RgbaImage;
use std::io::Read;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

fn snap_socket_path() -> PathBuf {
    let dir = std::env::var("XDG_RUNTIME_DIR").expect("XDG_RUNTIME_DIR not set");
    PathBuf::from(dir).join("waygriff-0.snap")
}

fn read_u32_le(stream: &mut impl Read) -> Result<u32> {
    let mut buf = [0u8; 4];
    stream.read_exact(&mut buf)?;
    Ok(u32::from_le_bytes(buf))
}

fn main() -> Result<()> {
    let path = snap_socket_path();
    let mut stream =
        UnixStream::connect(&path).with_context(|| format!("connecting to {}", path.display()))?;

    let width = read_u32_le(&mut stream)?;
    let height = read_u32_le(&mut stream)?;
    let stride = read_u32_le(&mut stream)?;

    let buf_size = stride as usize * height as usize;
    let mut bgra = vec![0u8; buf_size];
    stream.read_exact(&mut bgra)?;

    // BGRA → RGBA
    for row in 0..height as usize {
        let row_start = row * stride as usize;
        for x in 0..width as usize {
            let off = row_start + x * 4;
            bgra.swap(off, off + 2);
        }
    }

    // Strip padding if stride > width*4
    let row_bytes = width as usize * 4;
    let rgba: Vec<u8> = if stride as usize == row_bytes {
        bgra
    } else {
        (0..height as usize)
            .flat_map(|row| &bgra[row * stride as usize..row * stride as usize + row_bytes])
            .copied()
            .collect()
    };

    let img = RgbaImage::from_raw(width, height, rgba).context("bad image dimensions")?;
    let dyn_img = image::DynamicImage::ImageRgba8(img);

    let conf = viuer::Config {
        ..Default::default()
    };
    viuer::print(&dyn_img, &conf)?;

    eprintln!("{width}x{height} stride={stride}");
    Ok(())
}

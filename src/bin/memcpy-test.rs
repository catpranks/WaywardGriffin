use anyhow::{Result, bail};
use cudarc::driver::sys::{self, CUctx_flags};
use std::ffi::c_void;
use std::time::Instant;

// Helper to check for CUDA errors
fn check(result: sys::CUresult) -> Result<()> {
    if result != sys::CUresult::CUDA_SUCCESS {
        bail!("CUDA operation failed: {:?}", result);
    }
    Ok(())
}

fn main() -> Result<()> {
    // Initialize the driver
    check(unsafe { sys::cuInit(0) })?;

    // Get a device handle
    let mut device = 0;
    check(unsafe { sys::cuDeviceGet(&mut device, 0) })?;

    // Create a context
    let mut ctx = std::ptr::null_mut();
    check(unsafe {
        sys::cuCtxCreate_v2(
            &mut ctx,
            CUctx_flags::CU_CTX_SCHED_BLOCKING_SYNC as u32,
            device,
        )
    })?;

    let start_size: usize = 16 * 1024 * 1024; // 16MB
    let max_size: usize = 8 * 1024 * 1024 * 1024; // 8GB
    let mut size = start_size;

    while size <= max_size {
        println!("\n--- Testing size: {} MB ---", size / (1024 * 1024));

        // Allocate device memory
        println!(
            "Allocating two {}MB buffers on the device...",
            size / (1024 * 1024)
        );
        let mut d_src = 0;
        let mut d_dst = 0;
        check(unsafe { sys::cuMemAlloc_v2(&mut d_src, size) })?;
        check(unsafe { sys::cuMemAlloc_v2(&mut d_dst, size) })?;
        println!("Device allocation complete.");

        // Allocate host memory and get a pointer to it
        println!("Allocating {}MB of host memory...", size / (1024 * 1024));
        let host_data = vec![42u8; size];
        let h_src = host_data.as_ptr() as *const c_void;
        println!("Host allocation complete.");

        // Host to Device Copy
        println!("Starting H2D (Host-to-Device) copy...");
        let h2d_start = Instant::now();
        check(unsafe { sys::cuMemcpyHtoD_v2(d_src, h_src, size) })?;
        let h2d_time = h2d_start.elapsed();
        println!("cuMemcpyHtoD_v2 returned in {:?}", h2d_time);

        // Device to Device Copy
        println!("Starting DtoD (Device-to-Device) copy...");
        let d2d_start = Instant::now();
        check(unsafe { sys::cuMemcpyDtoD_v2(d_dst, d_src, size) })?;
        let d2d_time = d2d_start.elapsed();
        println!("cuMemcpyDtoD_v2 returned in {:?}", d2d_time);

        // Synchronize
        println!("Synchronizing context...");
        let sync_start = Instant::now();
        check(unsafe { sys::cuCtxSynchronize() })?;
        let sync_time = sync_start.elapsed();
        println!("cuCtxSynchronize returned in {:?}", sync_time);

        // Free device memory
        check(unsafe { sys::cuMemFree_v2(d_src) })?;
        check(unsafe { sys::cuMemFree_v2(d_dst) })?;

        size *= 2;
    }

    // Destroy the context
    check(unsafe { sys::cuCtxDestroy_v2(ctx) })?;

    Ok(())
}

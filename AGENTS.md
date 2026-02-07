# WaywardGriffin

Local display proxy: captures X11 via NVFBC, presents to Wayland window, forwards input back to X11.

## Thread model

| Thread | Entry | Does |
|--------|-------|------|
| Main | `lib.rs:run()` | Runs plotter TUI |
| Display | `display/mod.rs:run()` | Wayland window, surface, compositor events, input handling |
| Capture | `capture/mod.rs:Capture::run()` | Vulkan pipeline, frame render, swapchain present |

Communication: `mpsc::Sender<CaptureCommand>` from display→capture. Lock-free `ArcSwap<GlobalStateInner>` for shared state.

## Key files

- `capture/source/mod.rs` - `CaptureBackend` trait
- `capture/source/nvfbc/` - NVFBC capture, CUDA→Vulkan DMA-BUF, buffer pool
- `capture/input/mod.rs` - `InputBridge` trait (input injection + clipboard)
- `capture/input/xinput.rs` - X11 XTest input + clipboard via x11-clipboard
- `sizer.rs` - Coordinate transforms between source/window/render space
- `display/input.rs` - Shortcuts: Super+Escape (grab), Super+R (force relative), Super+C (capture toggle)

## Data flow

```
NVFBC → CUDA device mem → 2D memcpy → Vulkan image (DMA-BUF) → fragment shader → swapchain → Wayland
```

3-frame in-flight buffering. NVFBC buffer only valid until next capture call, so sync copy required.

## Build

Nix flake with crane. `nix build` or `nix develop` + `cargo build`.

C code in `src/c/nvcapture.c` compiled via build.rs (cc crate).

## Gotchas

- Capture and display are independent: capture backend connects to the *source* (X11, Wayland compositor to capture from, etc.), display thread connects to the *destination* compositor. These are separate connections, possibly to entirely different systems. Don't conflate them.
- No cross-vendor DMA-BUF: can't export Nvidia buffer to AMD iGPU directly
- Wayland syncobj protocol incompatible with Vulkan timeline semaphores (spec bug)
- NVFBC blocking capture timing is inconsistent (250-700us in windowed, oscillates in fullscreen)
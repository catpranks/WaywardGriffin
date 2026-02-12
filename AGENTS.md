# WaywardGriffin

Local display proxy: captures frames from a source display (X11 via NVFBC, or Wayland via screencopy/imagecopy), presents to a Wayland window, forwards input back to the source.

## Thread model

| Thread | Entry | Does |
|--------|-------|------|
| Main | `lib.rs:run()` | Runs plotter TUI |
| Display | `display/mod.rs:run()` | Wayland window, surface, compositor events, input handling |
| Capture | `capture/source/nvfbc/mod.rs:run()` or `capture/source/wayland/mod.rs:run()` | Backend-specific capture loop + SwapchainRenderer for Vulkan present |
| WaylandInput | `capture/input/wayland.rs` | Virtual keyboard/pointer/clipboard dispatch (wayland capture backends only) |

Communication: wakeup channel from display→capture (`mpsc` for nvfbc, `calloop_channel` for wayland backends). Lock-free `ArcSwap<GlobalStateInner>` for shared state.

## Key files

- `capture/mod.rs` - `SwapchainRenderer` (Vulkan pipeline, render, present)
- `capture/source/mod.rs` - `setup_and_spawn`, `DeviceId`, `BackendType`
- `capture/source/nvfbc/` - NVFBC capture, CUDA→Vulkan DMA-BUF, buffer pool
- `capture/source/wayland/mod.rs` - Wayland capture orchestration, calloop event loop, DMA-BUF buffer management
- `capture/source/wayland/screencopy.rs` - wlr-screencopy dispatch
- `capture/source/wayland/image_copy.rs` - ext-image-copy-capture dispatch
- `capture/source/wayland/dmabuf_probe.rs` - One-shot dmabuf feedback query
- `capture/input/mod.rs` - `InputBridge` trait (input injection + clipboard)
- `capture/input/xinput.rs` - X11 XTest input + clipboard via copypasta
- `capture/input/wayland.rs` - Wayland virtual keyboard/pointer input + clipboard via ext-data-control
- `sizer.rs` - Coordinate transforms between source/window/render space
- `display/input.rs` - Shortcuts: Super+Escape (grab), Super+R (force relative), Super+C (capture toggle)

## Data flow (NVFBC)

```
NVFBC → CUDA device mem → 2D memcpy → Vulkan image (DMA-BUF) → fragment shader → swapchain → Wayland
```

3-frame in-flight buffering. NVFBC buffer only valid until next capture call, so sync copy required.

## Data flow (Wayland)

```
screencopy/imagecopy → DMA-BUF → Vulkan image (imported) → fragment shader → swapchain → Wayland
```

## Build

Nix flake with crane. `nix build` or `nix develop` + `cargo build`.

C code in `src/c/nvcapture.c` compiled via build.rs (cc crate).

## Gotchas

- Capture and display are independent: capture backend connects to the *source* (X11, Wayland compositor to capture from, etc.), display thread connects to the *destination* compositor. These are separate connections, possibly to entirely different systems. Don't conflate them.
- No cross-vendor DMA-BUF: can't export Nvidia buffer to AMD iGPU directly

# Tasks

- [x] Modifiers parsing negotiation for ext-image-copy
  - deprioritize DRM_FORMAT_ARGB8888, DRM_FORMAT_XRGB8888 because wlroots adds them as fallbacks
- [x] Safety render on timeout even without wakeup
- [x] Wayland clipboard
- [ ] Input injector (libei)
- [x] Input injector (virtual keyboard/pointer)

# WaywardGriffin

Local display proxy: captures X11 via NVFBC, presents to Wayland window, forwards input back to X11.

## Thread model

| Thread | Entry | Does |
|--------|-------|------|
| Main | `lib.rs:run()` | Runs plotter TUI |
| Display | `display/mod.rs:run()` | Wayland window, surface, compositor events, input handling |
| Capture | `capture/source/nvfbc::run()` or `screencopy::run()` | Backend-specific capture loop + SwapchainRenderer for Vulkan present |
| WaylandInput | `capture/input/wayland.rs` | Virtual keyboard/pointer dispatch (wayland capture backends only) |

Communication: `mpsc::Sender<()>` wakeup from display→capture. Lock-free `ArcSwap<GlobalStateInner>` for shared state.

## Key files

- `capture/mod.rs` - `SwapchainRenderer` (Vulkan pipeline, render, present)
- `capture/source/mod.rs` - `setup_and_spawn`, `DeviceId`, `BackendType`
- `capture/source/nvfbc/` - NVFBC capture, CUDA→Vulkan DMA-BUF, buffer pool
- `capture/source/screencopy.rs` - screencopy capture, calloop-driven
- `capture/input/mod.rs` - `InputBridge` trait (input injection + clipboard)
- `capture/input/xinput.rs` - X11 XTest input + clipboard via x11-clipboard
- `capture/input/wayland.rs` - Wayland virtual keyboard/pointer input injection (WIP)
- `capture/source/wayland/dmabuf_probe.rs` - One-shot dmabuf feedback query
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

# Tasks

- [x] Modifiers parsing negotiation for ext-image-copy
  - deprioritize DRM_FORMAT_ARGB8888, DRM_FORMAT_XRGB8888 because wlroots adds them as fallbacks
- [x] Safety render on timeout even without wakeup
- [ ] Wayland clipboard
- [ ] Input injector (libei)
- [ ] Input injector (virtual keyboard/pointer)

## Wayland input injector (virtual keyboard/pointer) — WIP

### Architecture

Three Wayland connections to the source compositor for wayland capture backends:
1. `dmabuf_probe.rs` — one-shot connection to query dmabuf feedback for device selection
2. `capture/source/wayland/mod.rs:run()` — capture event loop (screencopy or image-copy)
3. `capture/input/wayland.rs` — input injection via virtual keyboard/pointer

All three use the same source socket (`opts.display`). Helper `connect()` in `capture/source/wayland/mod.rs`.

### Current state

Scaffolding complete. `WaylandInput` connects, binds managers, creates virtual devices, spawns dispatch thread. `InputBridge` methods are no-op. Wired into wayland capture backend (replaces `DummyInput`).

### Protocols

- `zwp_virtual_keyboard_v1` (wayland-protocols-misc, re-exported via `smithay_client_toolkit::reexports::protocols_misc::zwp_virtual_keyboard_v1`)
  - Manager v1: `create_virtual_keyboard(seat)` → virtual keyboard
  - **Must call `keymap(format=1, fd, size)` before any key/modifiers requests** — send XKB keymap as memfd
  - `key(time, key, state)` — key is evdev keycode, state 1=pressed/0=released
  - `modifiers(mods_depressed, mods_latched, mods_locked, group)`
- `zwlr_virtual_pointer_v1` (wayland-protocols-wlr, re-exported via `smithay_client_toolkit::reexports::protocols_wlr::virtual_pointer::v1`)
  - Manager v1-2: `create_virtual_pointer(seat)`
  - `motion(time, dx, dy)` — relative, wl_fixed_t (24.8 fixed-point)
  - `motion_absolute(time, x, y, x_extent, y_extent)` — needs source resolution for extents
  - `button(time, button, state)` — evdev button code, 1=pressed/0=released
  - `axis(time, axis, value)` + `axis_discrete(time, axis, value, discrete)` — axis 0=vert/1=horiz
  - `frame()` — groups events

Neither protocol has server→client events.

### Remaining work

- **Keymap**: virtual keyboard requires an XKB keymap fd before accepting keys. Correct approach: bind `wl_keyboard` on the source compositor's seat, receive its `keymap` event, forward that fd/contents to the virtual keyboard. This ensures layout matches the source.
- **Key/modifier forwarding**: display thread sends evdev keycodes via `event.raw_code` — pass through directly. Need to track and forward XKB modifier state.
- **Pointer implementation**: `motion`/`motion_absolute`/`button`/`axis` + `frame()` calls. `motion_absolute` needs source resolution from `GlobalState`/`Sizer` for x_extent/y_extent. Scroll: display sends `value120`, needs conversion to `axis`/`axis_discrete`.
- **Cross-thread communication**: `InputBridge` is called from display thread, virtual devices live in the input thread. Options: (a) proxy objects are `Send+Sync` in wayland-client 0.31+, could hold them in `WaylandInput` and flush from display thread; (b) channel-based command dispatch to the input thread.
- **Clipboard**: separate task, not part of virtual keyboard/pointer.
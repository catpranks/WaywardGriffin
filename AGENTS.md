<!-- deeds:begin -->
If present, use the `deeds` tool and skill for task tracking.
<!-- deeds:end -->

# WaywardGriffin

`waygriff` is a local display proxy: it captures frames from a source display (X11 via NVFBC, or a Wayland compositor via screencopy/image-copy), presents them in a Wayland window, and forwards keyboard/pointer/clipboard back to the source. The owner runs it to play X11 games on `:80` (rendered on an Nvidia card) inside a Hyprland session on an AMD iGPU.

- Vibe coded project: agents make commits as needed.
- Everything below this line is agent-maintained. Keep it current when you change the layout, sockets, or workflow; the owner reads it to check your work, not to learn the code.

## Working on it

- NVFBC is the primary path and the only one verified end to end. Wayland capture backends must keep building; nothing more is promised.
- `cargo clippy --release --all-targets` and `cargo fmt` before committing. Clippy does not link binaries; run `cargo build --release` before runtime checks, or `waygriffctl start` builds for you.
- Runtime check: `waygriffctl start && waygriffctl stats && waygriffctl snap && waygriffctl stop`. Read the PNG.
- Version control is jj colocated with git: `jj describe -m "..."` then `jj new`. Close the matching deeds issue in the same change.
- Session narrative goes to `docs/log.md`. Anything that changes what to do next goes to `.deeds/`.

## waygriffctl

`src/bin/waygriffctl.rs` is the agent-facing CLI. It exists so an agent can drive a live instance without a TUI or a focused window.

| Command | Does |
|---|---|
| `start [--no-build] [--workspace F1] [--no-fullscreen] [-- extra args]` | `cargo build`, write `temp/waygriff-launch.sh`, ask Hyprland to exec it on workspace `name:F1 silent` fullscreen (no focus change), wait for the display thread to answer `ping`, print stats. Logs append to `temp/waygriff.log`; PID in `temp/waygriff.pid`. |
| `stop` | `quit` over the control socket, wait for the socket to close, SIGTERM fallback after 5 s. Only signals a PID whose `/proc` cmdline is a `waygriff` binary. |
| `status`, `stats [--watch 1s]`, `state`, `set confine\|capture\|relative on\|off`, `raw <line>` | Control socket wrappers. Non-`ok` replies exit 1. |
| `snap [-o path] [--show]` | Screenshot of the current frame as PNG (default `temp/snap.png`). |
| `log [-n 40]` | Tail of `temp/waygriff.log`. |

Paths resolve against the checkout (`CARGO_MANIFEST_DIR`), overridable with `WAYGRIFF_DIR`.

Typical manual invocation: `cargo run --release --bin waygriff -- --backend=nvfbc --display=:80 --tdelay=1000ms` (the `--tdelay` flag turns on the TUI; without it logs go to stdout and `--stats-every 5s` prints JSON stats lines).

## Sockets

All abstract Unix sockets, one instance per machine for now (`waygriff-0`).

| Name | Kind | Purpose |
|---|---|---|
| `@waygriff-0.ctl` | stream, line protocol, JSON replies | `stats`, `state`, `set ...`, `ping`, `quit`, `help`. See `src/control.rs`. |
| `@waygriff-0.snap` | stream | Connect, read `u32 LE width, height, stride` then `stride*height` bytes of BGRA. |
| `@waygriff-0.input` | datagram, 12 bytes LE `{u32 type, f32, f32}` | type 0 = pointer delta, forwarded to the source. |
| `waygriff-0` (`$XDG_RUNTIME_DIR`) | Wayland server | Overlay compositor: a client's surface is composited over the captured frame (alpha blended). `overlay-client` bin is a vello demo client. |

## Threads

| Thread | Entry | Does |
|---|---|---|
| Main | `lib.rs:run()` | Loads config, spawns everything, then runs the monitor (TUI or headless log/stats printer). |
| Control | `control.rs:spawn()` | Accepts control socket connections, one thread each. Display-bound requests carry an ack channel and reply after the display thread applied them. |
| Display | `display/mod.rs:run()` | Wayland window, compositor events, input handling (`display/input.rs`), control requests, snapshot socket accept. Forwards frame callbacks to the render thread. |
| Render | `render/mod.rs:render_loop()` | Capture polling, Vulkan render, present, screenshots. |
| Overlay | `overlay/mod.rs` | Smithay-based Wayland server for the overlay surface. |
| Capture (Wayland backends only) | `source/wayland/mod.rs:run()` | screencopy/image-copy loop; frames land in a mutex slot. |
| WaylandInputDispatch (Wayland backends only) | `bridge/wayland.rs` | Clipboard dispatch on the source compositor. |

NVFBC has no capture thread: `CaptureBackend::capture()` runs synchronously on the render thread.

Communication: display→render via `mpsc::Sender<RenderMsg>` (frame signals, screenshot streams); control→display via calloop channel of `ControlReq`; shared flags in `ArcSwap<GlobalStateInner>` updated through `GlobalStateExt::update`; window/source geometry in `SharedSizer` (also `ArcSwap`). Stats flow render→monitor through `MonitorHandle` events; the monitor publishes a `stats::Snapshot` every 100 ms that the control thread serves.

## Layout

- `lib.rs` - CLI opts, `GlobalStateInner` + `GlobalStateExt`, thread spawn, tracing setup (log lines route through the monitor).
- `control.rs` - control socket protocol.
- `monitor.rs` - `Monitor` (main-thread consumer of render events), TUI (`ratatui`) and headless modes, `MonitorHandle` for producers.
- `stats.rs` - `FrameInfo` timings, rate windows, `Snapshot` (serde, what `stats` returns).
- `sizer.rs` - source/window/render coordinate transforms.
- `config.rs` - `~/.config/waygriff/config.toml` key block/remap (see `docs/key-filtering.md`).
- `display/mod.rs` - Wayland client boilerplate (sctk 0.21: `delegate_dispatch2!` plus `Dispatch2` impls on marker types), window, presentation feedback.
- `display/input.rs` - `InputState`: shortcuts (Super+Escape confine, Super+R force relative, Super+C capture toggle), pointer/keyboard forwarding, clipboard sync on confine toggle, input datagram socket.
- `render/mod.rs` - `Renderer`: Vulkan device/swapchain, per-frame command buffer, overlay blend, border effect, screenshot compute. Barrier helpers at the bottom.
- `render/shaders.rs` - GLSL sources and push-constant structs.
- `source/mod.rs` - `CaptureOpts`, `BackendType`, `CaptureBackend`/`CaptureBackendBuilder` traits, `CapturedFrame` (returns its buffer to the backend on drop).
- `source/nvfbc/` - NVFBC via `src/c/nvcapture.c`, CUDA→Vulkan opaque-fd images, buffer pool.
- `source/wayland/` - screencopy and image-copy backends, dmabuf feedback probe.
- `bridge/mod.rs` - `InputBridge` trait (inject input, get/set clipboards on the source).
- `bridge/xinput.rs` - XTest injection + X clipboards.
- `bridge/wayland.rs` - virtual keyboard/pointer + ext-data-control.
- `overlay/` - overlay compositor.
- `bin/waygriff.rs`, `bin/waygriffctl.rs`, `bin/overlay-client.rs`.

## Data flow

NVFBC:

```
NVFBC → CUDA device mem → 2D memcpy → Vulkan image (opaque fd) → fragment shader → swapchain → Wayland
```

Two frames in flight, Mailbox present mode. The NVFBC buffer is only valid until the next capture call, so the copy is synchronous.

Wayland backends:

```
screencopy/image-copy → DMA-BUF → Vulkan image (imported) → fragment shader → swapchain → Wayland
```

## Build

Nix flake with crane. `nix develop` gives a shell whose `LD_LIBRARY_PATH` carries the driver libs; the release binary needs that at runtime too (the launcher wrapper exports it). `nix build` also works.

## Hyprland (destination compositor)

- Hyprland 0.56 with a Lua config. `hyprctl keyword` and legacy dispatch strings are rejected; use `hyprctl eval '<lua>'`. Window rules at exec time: `hl.exec_cmd("cmd", { workspace = "name:F1 silent", fullscreen = true })`. Stubs for the API ship in the Hyprland package under `share/hypr/stubs/hl.meta.lua`.
- `HYPRLAND_INSTANCE_SIGNATURE` may be unset in agent shells; `waygriffctl` finds the newest `$XDG_RUNTIME_DIR/hypr/<sig>/.socket.sock`.
- The exec'd command goes through `sh -c`, so shell-quote before Lua-quoting.
- Workspace `F1` lives on the 4K@120 monitor. Fullscreen there without focus does not guarantee direct scanout; see the cursor-latency issue in `.deeds/`.

## Gotchas

- Capture and display are independent connections: the backend talks to the source (X11 or a Wayland compositor), the display thread talks to the destination compositor. Don't conflate them.
- No cross-vendor DMA-BUF: an Nvidia buffer cannot be handed to the AMD iGPU. Everything crosses through host-visible memory or stays on one device.
- `set_cursor` needs a pointer-enter serial; control-socket `set confine` before the pointer has entered must skip the cursor update.
- A panic on any thread exits the process (hook in `lib.rs`); otherwise a dead render thread leaves a frozen window and a live control socket.
- The control socket binds before the display thread exists. Use `ping` for readiness, not connect.

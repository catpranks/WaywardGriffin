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
- Headline latency measurements use `waygriffctl set confine on` (normal play skips the border). Compare unconfined once when changing rendering. Record `hyprctl monitors` and the Hyprland modeset-log count before/after each interval; stale feedback requires checking for hyprlock and stopping, not bypassing it.
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
| Display | `display/mod.rs:run()` | Wayland window, compositor events, input handling (`display/input.rs`), control requests, snapshot socket accept. Forwards frame callbacks and presentation timing to the render thread. |
| Render | `render/mod.rs:render_loop()` | NVFBC frame arrivals or Wayland capture polling, Vulkan render, present, screenshots. NVFBC uses the RADV iGPU; Wayland backends retain DMA-BUF device selection. |
| Overlay | `overlay/mod.rs` | Smithay-based Wayland server for the overlay surface. |
| Capture (NVFBC) | `source/nvfbc/mod.rs` | Blocking grab and synchronous CUDA D2H into pinned host pages imported by RADV, publish the latest frame and wake render. |
| Capture (Wayland backends only) | `source/wayland/mod.rs:run()` | screencopy/image-copy loop; frames land in a mutex slot. |
| WaylandInputDispatch (Wayland backends only) | `bridge/wayland.rs` | Clipboard dispatch on the source compositor. |

NVFBC captures on its own thread into one replaceable pending slot. The renderer permits one pending source presentation. For unscaled SDMA output with monotonic direct-scanout feedback it takes the newest frame 5 ms before the predicted next refresh; other NVFBC draws proceed on arrival/feedback. Wayland frame callbacks remain diagnostic and do not pace NVFBC. A 50 ms maintenance redraw handles static sources, overlays, and capture-off state. Capture uses a 50 ms grab timeout and its thread is joined on shutdown. Wayland capture backends retain callback-driven polling.

Communication: display→render via `mpsc::Sender<RenderMsg>` (frame signals, presentation timing, screenshot streams); control→display via calloop channel of `ControlReq`; shared flags in `ArcSwap<GlobalStateInner>` updated through `GlobalStateExt::update`; window/source geometry in `SharedSizer` (also `ArcSwap`). Stats flow render→monitor through `MonitorHandle` events; the monitor publishes a `stats::Snapshot` every 100 ms that the control thread serves.

## Layout

- `lib.rs` - CLI opts, `GlobalStateInner` + `GlobalStateExt`, thread spawn, tracing setup (log lines route through the monitor).
- `control.rs` - control socket protocol.
- `monitor.rs` - `Monitor` (main-thread consumer of render events), TUI (`ratatui`) and headless modes, `MonitorHandle` for producers.
- `stats.rs` - `FrameInfo` timings, rate windows, `Snapshot` (serde, what `stats` returns). Timing boundaries and accounting: `docs/latency.md`.
- `sizer.rs` - source/window/render coordinate transforms.
- `config.rs` - `~/.config/waygriff/config.toml` key block/remap (see `docs/key-filtering.md`).
- `display/mod.rs` - Wayland client boilerplate (sctk 0.21: `delegate_dispatch2!` plus `Dispatch2` impls on marker types), window, presentation feedback.
- `display/input.rs` - `InputState`: shortcuts (Super+Escape confine, Super+R force relative, Super+C capture toggle), pointer/keyboard forwarding, clipboard sync on confine toggle, input datagram socket.
- `render/mod.rs` - `Renderer`: Vulkan device/swapchain, SDMA upload for unscaled host captures, graphics command buffer, presentation deadlines, overlay blend, border effect restricted to four edge scissors, screenshot compute. In-flight fences retain capture frames and descriptor sets. Barrier helpers at the bottom.
- `render/shaders.rs` - GLSL sources and push-constant structs.
- `source/mod.rs` - `CaptureOpts`, `BackendType`, `CaptureBackend`/`CaptureBackendBuilder` traits, `CapturedFrame` (returns its buffer to the backend on drop).
- `source/nvfbc/` - NVFBC via `src/c/nvcapture.c`, CUDA D2H, buffer pool. `host.rs` owns aligned CUDA-registered pages imported with `VK_EXT_external_memory_host` as linear sRGB images with transfer-buffer aliases; it also performs their one-time transition to General before CUDA writes.
- `source/wayland/` - screencopy and image-copy backends, dmabuf feedback probe.
- `bridge/mod.rs` - `InputBridge` trait (inject input, get/set clipboards on the source).
- `bridge/xinput.rs` - XTest injection + X clipboards.
- `bridge/wayland.rs` - virtual keyboard/pointer + ext-data-control.
- `overlay/` - overlay compositor.
- `bin/waygriff.rs`, `bin/waygriffctl.rs`, `bin/overlay-client.rs`.

## Data flow

NVFBC:

```
NVFBC → Nvidia device mem → CUDA D2H over PCIe → pinned system pages / RADV buffer → AMD SDMA → AMD-native swapchain + overlay/border → Wayland / scanout
```

Two reusable frame resource sets, Mailbox present mode, one pending NVFBC source presentation with a 50 ms recovery timeout. The NVFBC buffer is only valid until the next capture call, so the capture thread completes the copy before grabbing again. CUDA and RADV share the host backing without a CPU pixel copy. Host images stay in General; CUDA publication and Vulkan barriers order writes before copying or sampling. Unscaled captures use SDMA; scaling retains image sampling. A semaphore orders SDMA before graphics; the swapchain is shared concurrently between those queues. Slots return to the pool only after renderer fences and frame references permit reuse. The renderer waits for GPU idle on shutdown. Allocation/driver constraints and measured limitations: `docs/cross-gpu.md`.

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
- Workspace `F1` lives on the 4K@120 monitor. Use `render:direct_scanout=2` (auto). The NVFBC path sets content type `game`; its AMD-native swapchain has reached direct scanout with ZERO_COPY feedback. The former Nvidia WSI path was rejected by AMD KMS and setting 1 caused blocking modesets; see `.deeds/closed/5jgf2q.md`. Pacing and measurements are explained in `docs/cross-gpu.md`.

## Gotchas

- NVFBC enables `RADV_EXPERIMENTAL=transfer_queue` before thread startup. Mesa 26.2.2 requires it for SDMA; absent a dedicated queue, uploads fall back to graphics and deadline pacing is disabled. The 5 ms deadline budget is measured for this iGPU at 4K@120, not an adaptive GPU timer.
- Capture and display are independent connections: the backend talks to the source (X11 or a Wayland compositor), the display thread talks to the destination compositor. Don't conflate them.
- Nvidia WSI exports linear system-memory DMA-BUFs that AMD can import for composition but rejects for scanout. This does not imply sharing Nvidia optimal-tiled VRAM images with AMD; see `.deeds/closed/5jgf2q.md`.
- `set_cursor` needs a pointer-enter serial; control-socket `set confine` before the pointer has entered must skip the cursor update.
- A panic on any thread exits the process (hook in `lib.rs`); otherwise a dead render thread leaves a frozen window and a live control socket.
- The control socket binds before the display thread exists. Use `ping` for readiness, not connect.

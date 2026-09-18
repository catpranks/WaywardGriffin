---
tier: objective
---
# Cursor latency investigation and optimization

User reports slightly laggy cursor movement in the game on `:80` through waygriff, attributing roughly half to the game and half to waygriff. Wants to look for optimization opportunities. Planned as its own session.

Observations from reading the code (unverified by measurement):

- The render thread is driven by `wl_surface.frame` callbacks from the destination compositor. Each callback triggers one NVFBC `NOWAIT` grab. A frame produced just after a grab waits up to one full compositor period before it is picked up, then another period until presentation. Fullscreen direct scanout in Hyprland removes one copy but not this scheduling gap.
- `Backend::capture` does a synchronous CUDA 2D memcpy plus stream sync on the render thread before recording the Vulkan command buffer.
- Input path: Wayland relative pointer -> `XInput::mouse_delta` -> XTest fake motion with a flush per event. Sub-pixel deltas are accumulated and truncated; at high polling rates small deltas can be delayed until they add up to a whole pixel.
- Present mode is Mailbox with 2 in-flight frames; `blank()` and `render()` both wait on the in-flight fence before acquiring.

Candidate experiments: blocking NVFBC grab on a dedicated capture thread with the render thread presenting as soon as a frame lands (decoupled from frame callbacks); measure with the presentation-feedback latency that `FrameInfo::present` already records; compare windowed vs fullscreen.

User note: the efficient fullscreen presentation path in Hyprland (direct scanout) may only kick in when waygriff also holds input (focused, grab active). The launcher places the window on `F1` fullscreen without focus, so stats gathered that way may not reflect the scanout path. Check `hyprctl monitors` / Hyprland debug output for scanout state when measuring.

Measured 2026-09-18 via `waygriffctl stats` (window on F1 fullscreen, not focused, game at main menu showing 120 FPS on :80; window 3072x1728 logical, render 3840x2160):

| metric | value |
|---|---|
| render_hz | 50-61 |
| miss_hz (NVFBC NOWAIT returned no new frame) | 62-78 |
| present_ms avg | 27 |
| present_ms min/max | 20 / 56 |
| wait+obtain+commit | < 1 ms |

Render rate is a fraction of the source's 120 Hz and present latency is over two 120 Hz periods, so the scheduling gap dominates, not the copy. Check whether the fullscreen window is actually on direct scanout in this configuration before drawing conclusions.

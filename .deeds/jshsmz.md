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

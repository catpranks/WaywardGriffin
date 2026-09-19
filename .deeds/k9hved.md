---
tier: freezer
---
# Tune the 5 ms render-ahead budget; consider making it a flag

`RENDER_AHEAD_NS` in `src/render/mod.rs` (render loop) schedules the SDMA draw
5 ms before the predicted next refresh. Measured GPU work on the RAPHAEL_MENDOCINO
iGPU at 4K@120 is about 1.3 ms (SDMA upload + confined graphics), so roughly
3 ms of the ~10 ms confined mean is slack. Each ms trimmed is a ms off average
present latency; the cost of trimming too far is a missed flip (one extra
refresh, 8.3 ms) on that frame.

Bounds: floor about 3 ms (GPU 1.3 ms + Hyprland pickup and atomic commit), so
4 ms is the realistic trim and 3 ms is marginal. The graphics pass shares the
GFX ring with Hyprland compositing the 1440p monitor; SDMA has its own engine.

Worth trying if ever needed: 3 ms and 4 ms in 4-minute confined windows
(`waygriffctl set confine on`, `waygriffctl stats`), watching `present_ms` mean
and max together with `zero_copy_fraction` and `discard_hz`. If the sweet spot
depends on load, a `--render-ahead` flag beats another constant.

The budget only applies on the SDMA path (unscaled output, dedicated transfer
queue present). Scaled and Wayland-capture draws submit immediately.

Parked 2026-09-18 by the owner: not worth a millisecond of ricing. The whole
deadline scheme is superseded once VRR is available on the AMD HDMI output;
see the VRR refactor issue. Revisit only if VRR slips or the constant starts
missing flips.

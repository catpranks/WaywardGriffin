# Latency stats

`waygriffctl stats` reports rates over the last two seconds and timing averages
for the last 600 displayed source frames in headless mode (the TUI uses its
chart width). Timing history remains visible when presentation stops;
`feedback_age_ms` identifies stale feedback. Rates count events over the whole
observation window, including quiet periods.

`present_ms` uses the compositor's presentation timestamp, not the time its
feedback reaches the application. It estimates source-frame-to-presentation
latency; physical panel response and scanout position are not measured. NVFBC's
source timestamp is when the display server started rendering the source frame.
The compositor's clock must be `CLOCK_MONOTONIC` for cross-clock durations;
unsupported or missing measurements are null, and `present_frames` counts valid
source-to-presentation samples. The legacy total/min/max fields are zero without
valid samples.

For NVFBC, these averages form the additive breakdown:

```
present_ms = age_at_grab_ms + grab_to_submit_ms + submit_to_present_ms
```

| Field | Boundary / interpretation |
|---|---|
| `age_at_grab_ms` | Source timestamp → NVFBC grab return. |
| `wait_ms` | Time inside the blocking grab call, mostly waiting for the next source frame. This precedes that frame's lifetime; do not add it to source age. |
| `obtain_ms` | Grab return → copied Vulkan image ready, including buffer selection/allocation and synchronous CUDA D2H into RADV-imported host pages. |
| `grab_to_submit_ms` | Grab return → start of Vulkan queue submission. |
| `render_queue_ms` | Image ready → renderer starts handling it. Included in `commit_ms`. |
| `fence_wait_ms`, `acquire_ms` | CPU wait for the reusable frame resource and for a swapchain image. Included in `commit_ms`. |
| `commit_ms` | Image ready → start of queue submission. Legacy name; does not time the Wayland surface commit. Remaining time after queue/fence/acquire is CPU render preparation. |
| `queue_submit_ms` | CPU duration of Vulkan queue submission; does not measure GPU execution. |
| `queue_present_ms`, `queue_present_max_ms` | CPU time inside the Vulkan present wrapper. |
| `submit_to_present_ms` | Submission start → compositor-reported presentation. Includes GPU/driver/WSI/compositor delays; those are not individually resolved. Overlaps `queue_submit_ms` and `queue_present_ms`. |
| `present_return_to_present_ms` | Present wrapper return → presentation. Signed because presentation may happen before the call returns. |
| `feedback_delay_ms` | Actual presentation → feedback received by the display thread. Excluded from `present_ms`. |
| `frame_callback_hz`, `frame_callback_interval_ms` | Received Wayland frame callback rate and mean interval. Diagnostic on NVFBC; still paces the Wayland capture backends. |
| `render_idle_ms` | Render loop wait for a frame/control message; not an additive part of a displayed frame's age. |

All per-frame timing averages describe displayed source frames.
`capture_hz` counts new NVFBC frames and `miss_hz` counts backend-reported misses.
`capture_drop_hz` counts pending captured images replaced before rendering.
`submit_hz` counts nonblank submissions, including repeated source frames;
`render_hz` counts displayed new source frames.
`discard_hz` counts discarded presentations carrying source timing;
`feedback_timeout_hz` counts pending records expired when subsequent feedback is
requested.

`direct_capture_fraction` reports NVFBC `bDirectCapture` over displayed frames.
`refresh_ms` is the output refresh period from the latest presentation feedback;
the TUI scales its chart axis and draws guides in whole periods of it.
`presentation_flags` is the latest raw `wp_presentation_feedback` bitmask:
1 = VSYNC, 2 = HW_CLOCK, 4 = HW_COMPLETION, 8 = ZERO_COPY.
`zero_copy_fraction` reports bit 8 over displayed frames. ZERO_COPY means the
client buffer reached display hardware without compositor copying, including
direct scanout or a hardware overlay. It is independent of NVFBC direct capture.

NVFBC grabs on a dedicated thread with a 50 ms timeout, completes the CUDA copy,
and publishes the image into one replaceable slot. Only the empty→occupied
transition wakes rendering, so slow rendering does not create a FIFO of stale
frames. The renderer allows one pending source presentation, with a 50 ms
maintenance fallback when feedback is absent. For unscaled dedicated-SDMA output,
monotonic ZERO_COPY feedback supplies the next refresh estimate. Rendering starts
5 ms before that refresh and takes the newest completed capture at that point.
The wait remains visible in `render_queue_ms`; no timestamp boundary is moved to
hide it. Mailbox presentation remains asynchronous. Other NVFBC draws proceed
on arrival/feedback without this deadline. A single outstanding frame-callback
request reports callback delivery cadence; it does not pace NVFBC or measure
output refresh rate. Maintenance redraws handle static sources, overlays, and
capture-off state.

The NVFBC buffer is valid only until the next grab, so the copy is synchronous.
GPU references keep copied images out of the pool until rendering finishes.
The capture thread is stopped and joined on shutdown. Wayland capture backends
retain their callback-driven polling.

GPU execution and compositor scheduling remain aggregated in
`submit_to_present_ms`; the CPU fence and present-call timings cannot isolate them.
On the current Hyprland/Nvidia/AMD setup, use `render:direct_scanout=2` to avoid
failed direct-scanout attempts causing blocking modesets with the former Nvidia
WSI path (see `.deeds/closed/5jgf2q.md`). The NVFBC path now renders on RADV and
sets content type `game`, permitting auto scanout. Verify both
`directScanoutTo` and ZERO_COPY feedback; null policy blockers alone do not prove
successful scanout. See [cross-gpu.md](cross-gpu.md).

Take headline measurements with `waygriffctl set confine on`: this is normal
play and skips the breathing border. Compare an unconfined window when changing
rendering. Its four edge scissors limit the extra draw, but the two states still
perform different GPU work. Wait for the 600-frame timing window to refill after
each change. Save monitor state and count modeset log entries around every
measurement. If feedback stops, check for hyprlock and stop rather than bypassing
the lock.

The source and output clocks are not phase locked. Short consecutive windows can
differ by several milliseconds or wrap across a refresh boundary without a code
change. A small border-cost measurement needs GPU timestamps; subtracting two
short `present_ms` windows can mistake clock phase for rendering cost.

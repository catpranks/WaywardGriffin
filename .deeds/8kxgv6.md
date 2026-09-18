# Render and present on the AMD iGPU; NVFBC frames cross once via pinned host memory

Why: the Vulkan device is chosen by CUDA UUID, so waygriff renders on the RTX 5090 and the Nvidia WSI hands Hyprland (AMD iGPU) a linear system-memory DMA-BUF. AMD KMS rejects that buffer for scanout (foreign import lacks the GTT scanout flag), so the frame is always composited: about two refresh periods from submit to presentation, measured at 15 ms of the ~20 ms total. Feasibility and probe numbers are in 5jgf2q: CUDA device-to-host into `cuMemHostRegister`ed pages imported with `VK_EXT_external_memory_host` costs ~0.6 ms per 4K frame; RADV-native buffers pass `drmModeAddFB2WithModifiers`.

Shape: capture thread does the NVFBC grab and one D2H copy into a ring of host slots; the render thread on RADV samples the imported linear image, draws base + overlay + border into an AMD-native swapchain image, presents Mailbox; waygriff sets content type `game` so Hyprland (`direct_scanout = 2`) treats it as a scanout candidate. Expected: `zero_copy_fraction` 1.0 and submit_to_present near one refresh period. Not demonstrated yet: actual direct scanout of the AMD swapchain buffer under Hyprland, and the sampling cost of a 4K linear host image on the iGPU.

Side effect: the CUDA/Vulkan opaque-fd interop in `source/nvfbc/` goes away; the Nvidia side becomes CUDA-only.

Owner note: this is a guided tour as much as a change. The owner wants to follow how the physics of moving a frame between the two GPUs (where the bytes live, who copies them, over which bus, who waits on whom) maps onto the kernel and driver politics that decide it (DMA-BUF export/import rules, who may build a KMS framebuffer from whose memory, GTT vs VRAM domains, modifiers, sync objects, what the proprietary Nvidia stack hides). Whoever does this work writes it up for a reader who is curious, not in a hurry, and expects the dmabuf interop story to be ugly: state each rule that shaped a design decision, cite the source line or probe that established it, and keep the walkthrough in `docs/` (proposed: `docs/cross-gpu.md`) with the design, not in the issue. 5jgf2q already holds the raw findings to build from.

## Implementation baseline (2026-09-18)

Clean starting revision c8e1455f; fresh measurement with the same HoN menu at
3840×2160, 120 FPS. Hyprland direct_scanout=2, F1 visible at 3840×2160@120,
no hyprlock. Modeset count stayed 109907 before/after. Snapshot inspected.
600-frame baseline: render 120 Hz, capture 120.5 Hz, present 14.072 ms
(min 13.805, max 30.667), submit_to_present 13.528 ms, obtain 0.349 ms,
grab_to_submit 0.388 ms, age_at_grab 0.156 ms, zero_copy_fraction 0,
presentation flags 7. This fresh baseline differs from the earlier 12.5 ms
reference; asynchronous source/output phase can shift latency. Raw stats and
monitor snapshots are in temp/before-*.json (disposable).

Implementation: replace NVFBC's CUDA UUID device selection with RADV iGPU,
CUDA D2H into aligned host pages imported as a linear sRGB sampled image.
Vulkano 0.35.2 lacks host-pointer import in MemoryImportInfo: use its raw
Vulkan function table for that allocation, then wrap the handle. Keep lifetime
ownership of host pages through Vulkan image references; synchronize CUDA
before publishing and fence AMD reads before reuse. Wayland capture retains
its existing DMA-BUF device selection and external queue ownership barriers.

First AMD execution: RADV RAPHAEL_MENDOCINO selected; host import works with
sRGB linear sampling, type 5, row pitch 15360, offset 0. Four slots allocated.
Hyprland directScanoutTo matches waygriff, no blockers, flags 15 and steady
zero_copy_fraction=1. Modesets remain 109907. However initial present ~18 ms
and submit_to_present ~17 ms are worse than Nvidia. Do not accept this as the
performance result: investigate GPU duration vs WSI/compositor scheduling.
Screenshot shows correct colors/content. Raw initial data:
temp/amd-scanout-initial-stats.json.

Correctness findings during implementation: external VkImages must be created
Undefined (VUID-VkImageCreateInfo-pNext-01443), contrary to the feasibility
probe's Preinitialized create info. Initialize General before the first CUDA
write and retain General for host writes/sampling. Vulkano raw command buffers
do not retain descriptor sets: keep base/overlay descriptors with the in-flight
fence. Wait for GPU idle before renderer destruction/host-page release.

Performance isolation: temporary GPU timestamp queries measured ~4.4–4.8 ms
for direct host sampling + border, ~3.8–4.2 ms without border; steady D2H obtain
~0.89 ms. Separate opaque base pipeline gave no measurable GPU improvement.
Copying into an intermediate optimal AMD texture then sampling made GPU time
~5.2 ms; rejected. Testing an unscaled host-image → swapchain copy instead of
base sampling (overlay/border remain shader draws); scaled windows retain
sampling. This keeps one Nvidia→host crossing and no extra full-frame scratch
image.

Mesa 26.2.2 source obtained from archive.mesa3d.org (~/src/mesa-26.2.2/src/vulkan/wsi);
the old ~/src/mesa checkout is incomplete. WAYLAND_DEBUG trace confirms one
feedback request followed immediately by syncobj acquire/release points and
surface commit per submission, not delayed WSI thread commits. Hyprland code
at the running commit schedules DS through Aquamarine's page-flip scheduler.
A/B debug:ds_handle_same_buffer=false worsened cadence/latency; restored true.
One confine/no-border run saw the cumulative modeset count rise by 91 to
109998 during its interval, then remain stable. Exclude that interval from
comparisons; later monitor and mode-count checks remain necessary.

Owner steering: headline numbers must use `set confine on` (normal play skips
border); compare confined/unconfined side by side. If border cost is material,
restrict it to four edge strips. Preliminary direct-sampling GPU timestamps
showed ~0.5–0.7 ms for the full-viewport blended border, so optimize it.

Experiments beyond direct sampling (still uncommitted, may be removed): a
host image -> native swapchain copy reduced GPU time to ~3.6–3.8 ms with border,
versus ~4.6 ms for sampling; staging into an intermediate image, host-buffer
alias upload on graphics, and waiting the render fence before queue_present
were not improvements. All still had source/output phase-sensitive latency
~15–20 ms unconfined. Auto iGPU clocks are dynamic (600 then 2200 MHz); do not
attribute performance to a fixed 600 MHz. Temporary high-clock test was denied
by sudo password, no power state changed. Same-buffer debug toggle restored
true; direct_scanout restored 2 after compositing test (also no improvement).

The SDMA experiment used temporary GPU query probes: RADV hides
transfer queue behind RADV_EXPERIMENTAL=transfer_queue. Normal queue-family
selection found only graphics and compute, so first transfer attempt actually
used graphics. Actual SDMA startup then hit VkCmdResetQueryPool's queue-type
restriction; query reset must remain on graphics, not transfer-only queue.
Launch wrapper temp/waygriff-sdma.sh sets the flag for this process only.
Known-good versions: temp/render-pre-gpu-probe.rs (minimal sampling implementation,
including descriptor lifetime fix, no GPU query); temp/render-direct-sampling.rs
(sampling + temporary GPU queries + separate opaque base pipeline, which gave
no gain); temp/render-copy-before-present-wait.rs (image-copy fast path + queries,
no buffer alias/SDMA); temp/host-image-only.rs (host allocation before alias).
The buffer alias, second queue, upload fast path, opaque-base pipeline, GPU queries
and fence-before-present toggle were subsequently removed. The retained renderer
samples the imported linear image and scissors the border to four disjoint edge
strips. The cross-GPU guide is now in docs/cross-gpu.md; AGENTS.md and latency.md
describe this path. Issue remains open: the latency acceptance condition is unmet.

## Performance boundary and retained implementation

Actual SDMA upload plus confined graphics measured about 1.28 ms with timestamps
spanning both queues; the edge-strip border added about 0.03–0.04 ms, compared
with the old full-viewport border's roughly 0.5–0.7 ms. The experimental queue
requires RADV_EXPERIMENTAL=transfer_queue; without it, selection fell back to
graphics. This materially reduced GPU work but did not establish lower total
latency. Keeping the sampling implementation avoids carrying a second queue,
buffer/image aliasing and a driver experiment without achieving the objective.

Same-process SDMA confined A/B, seven-second washouts, zero modeset increments:

| Variant | Displayed Hz | Total ms | Submit→present ms | Obtain ms |
|---|---:|---:|---:|---:|
| Async present | 120 | 16.371 | 15.294 | 0.887 |
| Wait render fence before present | 118.5 | 16.671 | 15.592 | 0.891 |
| Async repeat | 120 | 16.963 | 15.888 | 0.889 |
| Unconfined, edge strips | 120 | 17.145 | 16.050 | 0.897 |

The gradual drift makes a small latency difference inconclusive. A later
fence-before-present window still had 15.38 ms from present return to actual
presentation, with just 1.59 ms from submission to present return. Thus GPU
completion alone does not explain the remaining delay.

Wayland trace, all from one steady SDMA frame with CPU fence wait enabled:
feedback request #82 at 22:43:56.211169, ready surface commit at .212978,
associated frame callback #43 received at .215407, presentation feedback #82
at .223781. Presentation timestamps were one refresh apart. Hyprland calls
surface.frame() from presentFeedback() during attemptDirectScanout; Aquamarine
permits one pending page flip. This localizes an extra interval beyond the draw,
but does not prove the exact compositor/atomic/kernel cause. No privileged DRM
trace was obtained; /sys/kernel/tracing is root-only and sudo -n is unavailable.
Raw wire trace is disposable temp/sdma-wire.log; inspected source revisions and
paths are documented in cross-gpu.md and the preceding investigation.

Other tests: misc:vrr=2 left HDMI-A-1 vrr=false (connector vrr_capable=0), restored
misc:vrr=0. debug:ds_handle_same_buffer=true and render:direct_scanout=2 restored.
Pausing/resuming capture did not eliminate the delay. No power policy changed.

Rebuilt c8e1455f in temp/baseline-src, preserving its executable at
temp/baseline-bin/waygriff. Clean seven-second, 600-frame windows:

| Path / state | Displayed Hz | Total ms | Submit→present ms | Obtain ms | ZERO_COPY |
|---|---:|---:|---:|---:|---:|
| Nvidia baseline, confined | 119.5 | 15.710 | 15.155 | 0.355 | 0 |
| Nvidia baseline, unconfined | 120 | 16.843 | 16.286 | 0.358 | 0 |
| Nvidia baseline, confined repeat | 119 | 10.310 | 9.757 | 0.359 | 0 |
| AMD retained sampling, confined | 120 | 16.315 | 15.227 | 0.886 | 1 |
| AMD retained sampling, unconfined strips | 120 | 17.512 | 16.421 | 0.894 | 1 |

Every listed interval had zero modeset-log growth (count 109998), expected monitor
geometry, live feedback and matching scanout state. A source screenshot was
requested during the first AMD confined washout; repeat a clean interval for
any acceptance decision. The source/output phase drift is large: do not subtract
the confined/unconfined total times to estimate border cost, or select the best
window as a claimed speedup. The GPU timestamp comparison establishes border cost.

Outstanding: correlate acquire-point readiness, compositor state promotion,
atomic commit and page-flip timestamps to explain the extra interval; compare
the resulting fix over full source/output phase coverage against confined
c8e1455f. Do not close this issue merely because direct scanout works. The owner
was asked whether to retain AMD as a documented milestone if the latency target
cannot be reached, versus retaining only the border optimization. No reply yet.

Final reviewed build, after framebuffer-lifetime fix and resize round trip:
confined 119 Hz, total 17.341 ms, submit→present 16.236 ms, obtain 0.890 ms;
unconfined 119 Hz, total 19.069 ms, submit→present 17.972 ms, obtain 0.883 ms.
Both 600-frame windows had ZERO_COPY=1, matching directScanoutTo and zero modeset
increments. Raw records: temp/amd-reviewed-{confined,unconfined}-*.json. No probe,
overlay, screenshot or build ran during these final measurement windows.

Release build, all-target release clippy, formatting and all three existing
library tests passed. Source and full-output PNGs were read; border, overlay
alpha composition, resized letterboxing, capture off/on and shutdown while
disabled worked. Wayland source backends compile but were not run. A larger
green rectangle in a disposable overlay-client variant made composition easy
to inspect; the production demo was restored. Final waygriff instance stopped.
Border subtask jwydmn is closed. AMD migration is retained as a documented
milestone, not a completed latency objective. SDMA experiment can be recovered
from disposable temp/sdma-experiment.patch, with executable temp/sdma-bin/waygriff.

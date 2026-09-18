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

## Admission and acquire-fence investigation after 5b590b24

The milestone is committed at 5b590b24; work continues in its fresh child.
Temporary presentation-admission probes currently modify display/render: send a
PresentationDone timestamp to render and optionally gate to one pending source
presentation. Scratch switches temp/probe-one-present and
temp/probe-fresh-present must not remain in production code. The latter now
contains a nanosecond age allowance relative to the last presentation timestamp;
zero means discard every capture older than that presentation.

Confined same-process sampling-path A/B, all zero modeset increments:
unrestricted 120 Hz / 14.997 ms total / 13.924 ms submit→present;
one pending, consume latest 119.5 Hz / 15.287 / 8.626;
unrestricted repeat 120 Hz / 15.417 / 14.344. Gating shortened submission delay
but shifted it into capture-slot age. Strictly fresh admission gave 60 Hz /
11.364 ms total / 10.271 ms submit→present, so is unacceptable by itself.

Reconstructed SDMA probe in disposable temp/sdma-admission from the saved patch,
with current admission loop. Its strict-fresh mode also gave 59–60 Hz at 9.16–9.41
ms total. Changing to consume-latest gave 120 Hz / 9.661 ms total / 8.573 ms
submit→present, then removing admission immediately gave 119.5 Hz / 17.181 /
16.114. This is one favorable phase window, not an acceptance result.

Client-only LD_PRELOAD probe temp/trace-acquire.c imports WSI's exported DRM
syncobj timelines into a separate render-node fd and waits for its acquire
points. It changes no display configuration or timeline signals. Correlated
7,741 frame callbacks, commits, acquire signals and presentation timestamps over
64.85 s; all syncobj waits succeeded. Off-gate example (900 frames): commit→ready
1.502 ms, ready→callback 3.403 ms, callback→presentation 9.141 ms. On-gate example
(500 frames): commit→ready 1.457 ms, ready→callback -0.641 ms (callback timestamp
has millisecond truncation), callback→presentation 7.054 ms, ready→presentation
6.413 ms. This rules out an extra full refresh hidden in WSI acquire readiness
for these runs. Files: temp/acquire-wire.log and temp/acquire-correlated.json.
The source timestamp still drifts relative to output; unrestricted and gated
short-window totals are not comparable without phase coverage.

Next probe: allow a recent pending capture at feedback, discard only older ones
when the next capture should still fit before the next output refresh. Test
sampling first (roughly 0.9 ms D2H + 4 ms GPU); avoid carrying the experimental
SDMA queue if pacing alone can meet the target. No admission change retained yet.

The age-discard rule is rejected. Sampling with 6 ms allowance averaged 115.1 Hz
across 185 s and dipped to 79.5 Hz. SDMA with 4 ms allowance reached a phase that
halved cadence to 60 Hz. Predicting a future capture and throwing away the only
available image is fragile even when GPU work is fast.

A simpler replacement keeps the capture slot until a render deadline: last
presentation timestamp + reported refresh period - 4 ms. This leaves SDMA/WSI
headroom and chooses the freshest available capture without predicting its
arrival. One source presentation remains outstanding at a time; a 50 ms fallback
allows maintenance/recovery when feedback stops. Deadline pacing is restricted
to matching monotonic, ZERO_COPY feedback and the unscaled dedicated-SDMA path.
The slower sampled/Wayland paths retain their existing behavior except NVFBC's
one-presentation limit. Current long probe is temp/sdma-deadline4-samples.json;
initial submit→present is about 4.1–4.4 ms. Full phase coverage and final cleaned
build validation are still required before accepting this implementation.

The four-minute deadline probe averaged 117.79 displayed Hz, 9.358 ms total,
4.182 ms submit→present, ZERO_COPY=1; modesets stayed 109998. Clean build recent
windows are 119–120 Hz, with a dip around source/output phase crossing. Review
found the deadline timeout was also redrawing the old source when no new frame
was ready, immediately before the next CaptureReady. Suppress that duplicate
unless the 50 ms maintenance interval or a configure requires a redraw. This
needs a fresh cadence measurement; it does not change timing boundaries.

Final implementation review: restrict the extra queue to NVFBC devices, retain
old framebuffer fence waits on resize, and keep buffer/image aliases alive with
the captured-frame owner. No temporary filesystem switches or GPU query probes
remain in src/. `cargo clippy --release --all-targets`, release build and the
three library tests passed before the duplicate-redraw correction; rerun after
that change. Docs now describe SDMA, its Mesa experimental flag, queue semaphore
ordering and deadline pacing. Final functional checks and comparison remain.

Final gated build (duplicate redraw suppressed) over 243 s: 116.32 displayed Hz,
9.384 ms total, 4.266 ms submit→present, ZERO_COPY=1. Unconfined 600-frame window:
116.5 Hz, 6.484/4.082 ms, ZERO_COPY=1. No modeset growth. The short unconfined
window is in a different source/output phase and cannot measure border cost.
The contemporaneous Nvidia baseline is nearer 119 Hz, so investigate the
remaining gated cadence loss before claiming completion.

Next small pacing experiment predicts the next deadline after submission and
uses feedback to correct it, rather than waiting for feedback before scheduling
every render. Predictions expire after 50 ms without valid scanout feedback;
other modes retain the one-presentation fallback. Gated source is saved in
temp/render-gated-final.rs. This addresses feedback-delivery jitter without
changing source timestamps or hiding capture-slot age. Baseline measurement
must finish before compiling or running the candidate.

Contemporaneous confined Nvidia c8e1455f baseline, 243 s: mean 118.457 displayed
Hz, 14.012 ms total, 13.439 ms submit→present, obtain 0.356 ms, ZERO_COPY=0.
Mode count remained 109998. A few cadence dips also affect the baseline, but
its mean is still above the final strict-gate run's 116.3 Hz.

The free-running deadline experiment restored about 120 Hz but also restored
one queued refresh: submit→present 12.32 ms and total ~15–17 ms. Rejected after
about 70 s; no mode-count growth. Restore gated pacing and increase render-ahead
budget from 4 to 5 ms, trading one millisecond for more WSI/KMS headroom. Also
clear an expired deadline while waiting at the fallback gate to avoid a busy
recv_timeout(0) loop if feedback is missing. New candidate measurement:
temp/amd-ahead5-confined-samples.json. Do not retain the free-running experiment.

Five-millisecond gated budget accepted for the final comparison: 243 s confined,
mean 118.880 displayed Hz, 10.146 ms total, 5.031 ms submit→present, 0.967 ms obtain,
ZERO_COPY=1 throughout. Min/max five-second-sampled rates 115.5–120 Hz; final
minute's sampled rates were 120 Hz. Nvidia baseline mean was 118.457 Hz. Total
rolling means span 6.184–14.166 ms with source/output phase; the full-run mean is
below the 12.5 ms objective. Submit→present rolling means stay 4.968–5.214 ms.
Mode count remains 109998 and monitor is 3840×2160@120 with native scanout.

Final all-target release clippy and three library tests pass. Functional checks
read source PNG, unconfined border + overlay PNG, scaled/letterboxed 1000×900
logical window PNG and capture-off gray PNG; capture resumed and fullscreen
scanout returned. A disposable large green translucent overlay made blending
visible; production overlay source/binary is restored. No debug instrumentation
is compiled into waygriff. Final paired long unconfined interval is running in
temp/amd-ahead5-unconfined-samples.json, after the overlay process exited.
Remaining: record that interval, verify shutdown while capture is disabled,
finish summary docs, close this issue and commit/finalize the jj change.

## Accepted result

| Path / state | Displayed Hz | Total ms | Submit→present ms | Obtain ms | ZERO_COPY |
|---|---:|---:|---:|---:|---:|
| Nvidia c8e1455f, confined | 118.457 | 14.012 | 13.439 | 0.356 | 0.000 |
| AMD SDMA, confined | 118.880 | 10.146 | 5.031 | 0.967 | 1.000 |
| AMD SDMA, unconfined strips | 118.076 | 10.202 | 5.118 | 0.985 | 1.000 |

Each comparison ran ~243 s; first 10 s excluded, means of five-second snapshots.
All monitor modes stayed 3840×2160@120 and modeset counts stayed 109998. Native
AMD scanout and ZERO_COPY=1 persisted. Confined latency is below 12.5 ms average,
submit→present is about 5 ms, and steady cadence reaches 120 Hz with a long-run
mean slightly above the Nvidia baseline. Unconfined border is four strips;
isolated GPU cost remains roughly 0.03–0.04 ms versus 0.5–0.7 ms originally.
The full guided explanation and final table are in docs/cross-gpu.md.

Final functional checks passed (PNGs read): source screenshot, translucent
overlay, four border edges, resize/letterboxing, capture off/on and fullscreen
scanout recovery. Shutdown while capture was disabled completed normally.
Release build, all-target release clippy, formatting and all three existing
library tests pass. Wayland capture backends compile; they were not run.
No compositor settings or power policy were changed by the retained solution.
The final waygriff instance is stopped. The original CUDA/Vulkan opaque-fd
interop is removed; Nvidia only captures and copies through CUDA. Mesa's
experimental SDMA queue and the fixed 5 ms budget are documented limitations.

Closed: Completed native RADV scanout with one CUDA D2H crossing, SDMA upload and presentation-paced rendering. Four-minute confined average: 10.146 ms total, 5.031 ms submit-to-present, 118.880 displayed Hz with steady 120 Hz samples, ZERO_COPY=1; Nvidia baseline 14.012/13.439 ms and 118.457 Hz. Paired unconfined average 10.202/5.118 ms. Border strips, overlay, screenshot, resize, capture toggles, shutdown and build checks passed; guided documentation updated.

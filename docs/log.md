# Log

Append-only. Newest entry last.

## 2026-09-18 refresh

- Dependencies upgraded across the board (sctk 0.21 migration done by Codex, vello 0.10/wgpu 29, x11rb 0.14, cudarc 0.19 stayed).
- Added the control socket, `stats` module and headless monitor mode so an agent can run waygriff without the TUI. `waygriffctl` wraps it and places the window on Hyprland workspace F1 fullscreen via `hyprctl eval`.
- Layout pass: `src/capture` split into `render/`, `source/`, `bridge/`; shaders moved to `render/shaders.rs`; barrier and descriptor-set duplication folded into helpers; `plotter` renamed `monitor`; `GlobalStateExt::update` replaces the `with_*` builders; `memcpy-test` bin and the dummy bridge deleted; `vello` bin renamed `overlay-client`.
- Codex review of the control/launcher commit found seven issues (PID reuse, unacknowledged `set`, launcher exit codes, stop racing start, readiness, shell quoting, Nix paths); all fixed.
- Removed `docs/.obsidian`. Docs and AGENTS.md rewritten for the new layout.
- Baseline stats recorded in the cursor-latency issue for the coming optimization session: render ~50 Hz against a 120 Hz source, present ~27 ms.

## 2026-09-18 latency instrumentation and capture-thread trials

- Added stats JSON boundaries for source age at grab, grab-to-submit, render queue,
  fence/acquire waits, queue-submit/present CPU time, actual presentation, and
  feedback delivery. The previous `present_ms` used feedback receipt time.
  Presentation-clock validation, ZERO_COPY and NVFBC direct-capture fractions,
  callback cadence, idle time, discard/timeout rates, and feedback freshness now
  expose the previously hidden part of the pipeline. Feedback can precede
  queue_present return, so the two measurements are joined before publication.
- Fixed rate accounting for sparse/batched events: dividing by first-to-last
  event duration could turn a single batch of missed frames into millions of Hz.
  Three tests cover timing arithmetic, absent/invalid timestamps, and batched rates.
- Implemented and tried blocking capture on a dedicated thread with a replaceable
  image slot. Source capture reached 120 Hz with no steady-state misses, but
  unrestricted submission raised total latency to 51.9 ms, including 16.7 ms of
  swapchain acquisition and 32.3 ms after submission. Queue-present CPU time
  remained ~0.03 ms. Capturing faster exposed presentation backpressure.
- Tried one outstanding presentation: consuming the pending capture gave 22.8 ms;
  waiting for a fresh capture initially gave 15.9 ms, then 20.0 ms at 38.6 displayed
  Hz on repetition. The immediately following callback baseline gave 17.4 ms at
  57.6 Hz. Earlier callback baselines were 20.0 and 21.4 ms. These phase-sensitive
  results did not justify retaining a structural change for a latency goal.
  Reverted all capture-thread/admission changes; retained instrumentation only.
  Device selection and WSI configuration were unchanged throughout.
- NVFBC reported direct capture throughout; presentation flags were 7 and
  ZERO_COPY was absent. Hyprland reported solitary fullscreen on F1/HDMI-A-1
  at 4K120, without direct scanout. Snapshots showed the expected 120-FPS game
  menu. Capture-off/resume and stop were exercised during the thread trial.
- Measurement tables appended to g6hzzz and z7jks9 without changing issue status.
  Raw JSON, PNGs, saved binaries and the discarded experiment patch are in
  disposable `temp/latency/`. Timing definitions are in `docs/latency.md`.
  Release build, all-target release clippy, formatting and three library tests pass.
  No commit was made.

- Final retained-build snapshot drifted to 25.57 ms at 39.5 Hz: 5.46 ms source
  age + 0.41 ms grab-to-submit + 19.71 ms submit-to-presentation. Another 2.37 ms
  elapsed before feedback receipt. Verified the additive identity from JSON,
  read the final PNG, and stopped the instance. No reliable latency reduction
  is claimed from these variable runs.

## 2026-09-18 capture-thread repeat with direct scanout set to auto

The earlier capture-thread results were confounded by the failed-scanout modeset
storm identified in `.deeds/5jgf2q.md`. Repeated all four variants with Hyprland
already at `render:direct_scanout=2`. Every measured interval had zero HDMI-A-1
modeset log entries; F1 remained fullscreen 4K120, solitary, with CONTENT blocking
scanout. No compositor option, Vulkan device, swapchain or present mode was changed.

Two runs per variant used 12 s warmup after readiness and 20 one-second stats
samples. The retained loop had a third run after cleanup. Means of the overlapping
600-frame snapshots were:

| Loop | Source age ms | Grab→submit ms | Submit→presented ms | Total ms | Displayed Hz | Total min–max ms |
|---|---:|---:|---:|---:|---:|---:|
| Callback | 3.385 | 0.394 | 15.557 | 19.336 | 119.50 | 9.890–35.208 |
| Thread, immediate arrival | 0.153 | 0.389 | 15.256 | 15.798 | 119.58 | 14.394–41.697 |
| Thread, one presentation outstanding, consume pending | 0.151 | 5.528 | 8.795 | 14.475 | 115.81 | 6.307–47.298 |
| Thread, one presentation outstanding, wait for fresh | 0.151 | 0.413 | 11.671 | 12.235 | 64.65 | 4.941–30.244 |

Retained immediate arrival rendering: about 3.5 ms lower average latency at the
same displayed rate, with acquisition under 0.01 ms and no sustained post-submit
backlog. The pending-frame gate bought ~1.3 ms more at the cost of frame drops;
waiting for fresh frames reduced cadence to 60–70 Hz. Maximum latency spikes did
not improve reliably, so no tail-latency improvement is claimed.

NVFBC now grabs/copies on a dedicated thread, keeps one replaceable pending frame,
and wakes rendering when that slot becomes occupied. CUDA copy completion and
existing GPU frame references protect image reuse. Callback requests are bounded
to one outstanding request, used for diagnostics. A 50 ms maintenance redraw
keeps static-source overlays and capture-off state responsive. Capture has a 50 ms
timeout; shutdown stops and joins the worker. Removed all experimental
presentation-feedback admission code. Wayland backends keep callback polling.

Release build, all-target release clippy, formatting and all three timing tests
passed. Runtime checks covered normal startup, `--nocapture` startup, capture-off
and resume (back to 120 Hz), screenshot contents, and stopping while disabled.
Both measured lifecycle stops completed in 1.20 s without the signal fallback.
Final instance stopped. Definitions/architecture docs updated. Measurement-only
appends made to g6hzzz and z7jks9; issue states unchanged. Raw samples, compositor
condition records, log deltas and variant fixtures are in `temp/latency-auto/`.
No commit was made.

## 2026-09-18 the 40 Hz loop was Hyprland's failed-scanout modeset storm

- The presentation-path investigation (5jgf2q) found the cause of the sub-60 Hz
  render loop outside waygriff: Hyprland attempts direct scanout of our Nvidia
  WSI buffer every frame, AMD KMS rejects the foreign linear import as a
  framebuffer, and the rollback marks the output format dirty, so aquamarine
  issues a blocking modeset commit on every composited frame. The Hyprland log
  had 108k `Modesetting HDMI-A-1` lines.
- A/B with the new instrumentation: `render:direct_scanout=1` gives 47-55 Hz
  and 69 ms spikes with ~60 modesets/s; 0 or 2 gives 118-120 Hz, zero modesets,
  present max 24-28 ms. Setting 2 (auto) only scans out content-type `game`
  surfaces, which waygriff does not set, so it is the per-window opt-out.
  Changed the home-manager source (`nix-home/modules/desktop/hyprland.nix`) to 2
  and applied it at runtime; not yet rebuilt.
- With the storm gone, the capture-thread experiment was repeated and retained:
  source age at grab drops from 3-7 ms to 0.15 ms. Verification run of the
  committed build: 118.5 Hz, present 12.5 ms avg / 12.1 min / 20.9 max.
- Session lock: hypridle/hyprlock were killed at the owner's request and the
  dead lock cleared with `hl.clear_crashed_lockscreen()`.
- Next: 8kxgv6, render and present on the AMD iGPU so direct scanout can engage
  (submit-to-present is now ~12-15 ms of the total, about 1.5-2 refresh periods
  of composition). Owner wants that work written up as a cross-GPU walkthrough.

## 2026-09-18 AMD scanout and border milestone; latency objective remains open

NVFBC now uses CUDA D2H into aligned, registered system pages imported as a RADV
linear sRGB image. Rendering/presentation selects the integrated RADV device;
the final swapchain remains AMD-native. Removed Nvidia Vulkan device selection,
CUDA opaque-fd external-memory import and mipmapped-array wrappers. Content type
game makes the surface eligible for Hyprland auto scanout. Live 4K120 checks
reported matching directScanoutTo, flags 15 and ZERO_COPY=1, with zero modeset-log
growth during the recorded comparisons.

The owner identified the unconfined fullscreen blended border as a significant
iGPU cost. GPU probes measured roughly 0.5–0.7 ms; four disjoint 25-pixel scissors
reduced the isolated pass to about 0.03–0.04 ms while preserving the animation.
All subsequent headline windows were confined; paired unconfined measurements
and the measurement policy are recorded in the issue and latency documentation.

The latency acceptance condition is not met. Confined c8e1455f windows gave
15.710 and 10.310 ms total, versus 16.315 and 18.142 ms in retained AMD sampling
windows at different source/output phases. These short windows cannot establish
a speedup. Full tables, compositor conditions and trace boundaries are in
8kxgv6. That issue stays open.

Tried opaque base rendering, optimal-texture staging, image/buffer copies into
the swapchain, a dedicated SDMA queue, and waiting for GPU completion before
present. Actual SDMA reduced upload plus confined graphics to about 1.3 ms but
did not remove the presentation delay. It needed RADV_EXPERIMENTAL=transfer_queue.
Removed those experiments and all GPU timing/temporary file toggles, retaining
the simpler sampled-image path. Hyprland direct_scanout=2,
debug:ds_handle_same_buffer=true and misc:vrr=0 are restored; no power policy
changed. The wire trace shows ready commits receiving a callback at one refresh
and presentation feedback at the next, without locating the exact remaining
compositor/kernel delay.

Added docs/cross-gpu.md as the guided allocation, transfer, synchronization and
scanout walkthrough; updated AGENTS.md and latency.md. Review fixed descriptor
lifetime during raw command execution and framebuffer lifetime during resize,
and added GPU-idle teardown before releasing imported host storage.

Release build, release all-target clippy, formatting and the three existing
timing tests passed. Read source and full-output PNGs; checked the breathing
border, 1000×800 logical window with letterboxing, capture off/on and shutdown
while disabled. The overlay demo completed repeated runs; a disposable variant
with a larger translucent green rectangle made alpha composition unambiguous
in the output PNG. The original demo binary was restored. Wayland capture
backends compile; no Wayland source compositor was exercised.

Final reviewed-build windows after a resize round trip: confined 119 Hz,
17.341 ms total / 16.236 ms submit-to-present; unconfined 119 Hz,
19.069 / 17.972 ms. Both reported ZERO_COPY=1 and zero modeset-log growth.
No overlay, screenshot or build overlapped these windows. The border's GPU cost
comes from the isolated timestamp probes, not subtraction of these phase-shifted
end-to-end numbers. The final instance was stopped. Closed border issue jwydmn;
the main latency objective remains open.

## 2026-09-18: native AMD scanout latency follow-through

The first host-sampling milestone enabled native scanout but missed the latency
objective. The follow-through recovered the dedicated SDMA upload experiment,
then traced WSI acquire timeline readiness from a client-only interposer. Acquire
points became ready about 1.5 ms after commit; the extra refresh was downstream
in the scanout schedule. Limiting source presentations removed the extra queue
but required choosing a recent capture closer to the next refresh.

Strict fresh-frame admission halved cadence in some source/output phases and
was rejected. Free-running render deadlines restored cadence but allowed an
extra queued refresh, so that experiment was rejected too. The retained design
waits for presentation feedback, schedules rendering five milliseconds before
the next reported refresh, and takes the latest complete capture at that time.
It never discards the only available capture in anticipation of a future one.
A 50 ms maintenance fallback handles static sources and missing feedback.

The renderer uses a transfer-buffer alias of each imported host image. SDMA
copies unscaled frames directly into the native swapchain; graphics loads the
result and adds overlay/border. Scaled and Wayland captures retain sampling.
Mesa 26.2.2 requires its experimental transfer-queue flag, enabled before thread
startup. Temporary query pools, filesystem switches and alternate pipelines
were removed. The owner-facing cross-GPU guide now covers these decisions and
the scheduling evidence in addition to allocation provenance and KMS rules.

Final four-minute comparison:

| Path / state | Displayed Hz | Total ms | Submit→present ms | Obtain ms | ZERO_COPY |
|---|---:|---:|---:|---:|---:|
| Nvidia c8e1455f, confined | 118.457 | 14.012 | 13.439 | 0.356 | 0.000 |
| AMD SDMA, confined | 118.880 | 10.146 | 5.031 | 0.967 | 1.000 |
| AMD SDMA, unconfined strips | 118.076 | 10.202 | 5.118 | 0.985 | 1.000 |

Mode count stayed 109998 throughout; native scanout was continuous in both AMD
states. Source, overlay, border, letterboxing and capture-off PNGs were read.
Capture resumed correctly and stopping while disabled completed normally.
All-target release clippy, formatting, release build and three existing library
tests passed. Wayland backends were compile-checked only. No retained compositor
or power-policy changes. Final instance stopped.

## 2026-09-18 relative pointer diagnosis

Measured Logitech evdev → Hyprland relative-pointer → XTest → `:80` on F1
at scale 1.25. Both Wayland delta fields equaled injected integer counts;
waygriff's logical-to-source transform added 25% gain. Removed that transform
from relative forwarding, retaining `event.delta` and the existing XTest bridge.
Eight counts now give eight pixels on either axis in either direction; 400
counts at 1 kHz give 400 pixels with no observed delivery tail. The source-pixel
socket was already exact. Findings and scope limits are in `.deeds/nf466r.md`.

Used forced relative mode because HoN reported a visible cursor. Both snapshots
showed the game. Before/after monitor records retained 4K120 at scale 1.25 and
the modeset-line count stayed 109997 through both intervals. Formatting,
all-target release clippy and release build passed. Pointer positions and focus
were restored, then waygriff was stopped. No commit. Disposable probe, JSON
measurements and filtered Wayland logs are in `temp/pointer-diag/`.

## 2026-09-18 TUI review and rework

Reviewed the `--tdelay` dashboard against a live NVFBC run at 4K@120, captured
at 180x50 and 100x30 through tmux. The chart's `1.2 × max` axis let one 30 ms
spike squash the 6 ms steady state into three rows; frames without feedback
plotted as 0 ms; the status line reused `C` for three things and omitted the
scanout health signals; the log pane had no level colour or timestamps, wrapped
mid-token and reset scroll on every line; Ctrl-C was swallowed; the published
snapshot followed `--tdelay`, so the control socket served second-old stats.

Reworked `monitor.rs`: axis in whole refresh periods (`refresh_ms` now comes
from presentation feedback) with dim guides, p99-based headroom, values above
the axis clipped to the top edge in red, a second cyan series for
submit→present, legend and axis title removed. Three labelled status lines with
fixed flag slots, the additive `age + submit + out` breakdown, zero-copy /
direct / feedback-age alerts. Logs now arrive as structured events through a
`MonitorLayer` tracing layer instead of formatted text: level colour, seconds
since start, word wrap via `Paragraph`, repeat collapse (`×N`), follow-or-pin
scrolling with a `↓ N more` marker, Ctrl-C quits. Poll and publish run at
100 ms; `--tdelay` only paces redraws. Per-slot host allocation lines are debug.

`waygriffctl start --tui` re-execs the launch script inside a detached tmux
session on `temp/tmux.sock`; Hyprland's exec-rule token survives into the tmux
server, so the window still lands on F1. `waygriffctl tui [--size] [--attach]`
reads or attaches. Verified: repeat collapse, PageUp marker, styling escape
codes, 100x30 layout, `stop` tears the tmux server down.

Present latency read 6.4 ms in the first run and 9.5–12 ms in later ones with
`render_queue_ms` growing from 0.3 to 5.8; the sum stayed `age + submit + out`
and zero-copy stayed 100%. That is the source/output phase drift already on
file (e4gsqx, hqfcr3), not a TUI cost; the main thread wakes 10×/s. Formatting,
all-target release clippy, release build and the three library tests passed.
Wayland backends compile-checked only. Instance stopped.

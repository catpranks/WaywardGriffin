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

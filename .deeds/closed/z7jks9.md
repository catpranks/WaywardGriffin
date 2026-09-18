# Dedicated NVFBC capture thread; render on frame arrival instead of frame callback

Render loop is gated on `wl_surface.frame` callbacks and does one NVFBC NOWAIT grab per callback. Baseline shows the loop at 40 Hz against a 120 Hz source and 120 Hz destination: two of every three source frames are dropped and each displayed frame has waited up to a full loop period before being grabbed. Push-model NVFBC with a blocking grab on its own thread, render as soon as a frame lands, Mailbox present: the frame callback should not be the clock.


## Measurements, 2026-09-18 capture-thread experiment

Same :80 120-FPS game, fullscreen F1 on HDMI-A-1 at 3840x2160@120; NVFBC direct
capture fraction 1, presentation ZERO_COPY fraction 0. Timings use actual
CLOCK_MONOTONIC presentation timestamps, excluding feedback delivery.
Each row is a warmed 32-second run (nine stats snapshots at uptime 16–32 s,
with overlapping 600-frame timing windows).

| Variant | Capture Hz | Displayed Hz | Age at grab ms | Grab→submit ms | Submit→presented ms | Total ms |
|---|---:|---:|---:|---:|---:|---:|
| Callback baseline A | 49.63 | 49.64 | 1.370 | 0.400 | 18.211 | 19.982 |
| Capture thread, unrestricted submissions | 120.00 | 61.33 | 0.153 | 19.511 | 32.258 | 51.922 |
| Thread, one outstanding presentation, consume pending frame | 120.02 | 59.50 | 0.153 | 7.136 | 15.486 | 22.775 |
| Thread, one outstanding presentation, wait for next fresh frame A | 120.06 | 54.17 | 0.158 | 0.423 | 15.280 | 15.861 |
| Callback baseline B | 53.75 | 53.74 | 3.873 | 0.406 | 17.156 | 21.435 |
| Same fresh-frame thread policy B | 120.06 | 38.56 | 0.159 | 0.412 | 19.464 | 20.034 |
| Callback baseline C, immediately afterward | 57.64 | 57.64 | 0.946 | 0.412 | 16.053 | 17.411 |

Unrestricted submission moved callback idle time into swapchain acquisition
(16.684 ms) and increased post-submit latency; queue_present itself remained
about 0.03 ms. One-outstanding presentation removed acquire blocking, but
consuming a pending frame added about 6.7 ms of source age. Waiting for fresh
capture reduced source→submit to ~0.6 ms yet did not reliably beat the callback
baseline end to end, and the repeat lost throughput (38.56 vs 57.64 Hz).
The experimental capture thread was reverted. No GPU selection, swapchain,
present mode, or WSI configuration was changed. Instrumentation is retained.
Capture-off/resume, snapshot contents, and clean stop were verified during the
thread trial. Full breakdown is also appended to g6hzzz; raw samples and a patch
against the session's parent are in disposable `temp/latency/`.

Final callback-only verification later measured 25.574 ms at 39.5 displayed Hz
(5.457 ms source age + 0.407 ms grab→submit + 19.710 ms submit→presented).
The callback baseline itself varied substantially across the session; the
capture-thread trials did not establish a stable latency/throughput advantage.


## Measurements, 2026-09-18: repeat with the modeset storm removed

Hyprland `render:direct_scanout=2` throughout. Each measured interval appended
zero `Modesetting HDMI-A-1` lines; HDMI-A-1 stayed at 3840x2160@120, workspace
F1, waygriff solitary, `directScanoutTo=0`, blocker CONTENT. NVFBC direct capture
fraction was 1 and presentation ZERO_COPY fraction 0. No GPU, swapchain, present
mode, or compositor configuration was changed during these measurements.
The previous experiments above were confounded by failed-scanout modesets and
must not be used to judge capture-thread performance under this condition.

Each run used `waygriffctl start --no-build`, 12 s warmup after readiness, then
20 stats snapshots spaced 1 s apart, and `stop`. Timing windows contain the last
600 displayed frames and overlap. Table means average these snapshots equally;
extrema are the minimum/maximum observed in those windows, not percentiles.
Two runs per variant; the retained loop includes a third verification run after
removing unused experimental presentation-gating code.

| Loop | Runs | Age at grab ms | Grab→submit ms | Submit→presented ms | Total ms | Render Hz | Total min–max ms |
|---|---:|---:|---:|---:|---:|---:|---:|
| Callback | 2 | 3.385 | 0.394 | 15.557 | 19.336 | 119.50 | 9.890–35.208 |
| Thread, render on arrival (retained) | 3 | 0.153 | 0.389 | 15.256 | 15.798 | 119.58 | 14.394–41.697 |
| Thread, one outstanding presentation, consume pending | 2 | 0.151 | 5.528 | 8.795 | 14.475 | 115.81 | 6.307–47.298 |
| Thread, one outstanding presentation, wait for fresh | 2 | 0.151 | 0.413 | 11.671 | 12.235 | 64.65 | 4.941–30.244 |

Unrestricted arrival rendering captured 120.0–120.1 Hz, with zero steady-state
backend misses and essentially no captures overwritten. Swapchain acquisition
averaged 0.007–0.009 ms and queue_present 0.022–0.025 ms; the previous 16.7 ms
acquisition backlog disappeared. Source age was 0.151–0.154 ms, compared with
callback-run means of 2.280 and 4.491 ms. Its per-run total means were 15.471,
15.080, and 16.843 ms; post-submit means were 14.941, 14.545, and 16.283 ms.
Callback total means were 20.314 and 18.359 ms, with post-submit 15.422 and
15.692 ms. The average improvement is source freshness, with no sustained
post-submit queue growth; isolated maximum latency did not improve reliably.

One-outstanding/pending-frame pacing dropped 3.85–4.55 captures/s and retained
4.75–5.56 ms of queue age before render. One-outstanding/fresh-frame pacing
reduced displayed cadence to 59.8–69.5 Hz, dropping 50.6–60.4 captures/s.
Unrestricted arrival rendering was retained: lower latency than callback pacing
while preserving ~120 Hz, without the extra feedback gate. Instrumentation and
Wayland callback polling remain. Full per-run JSON, compositor-condition records,
log deltas, and source/binary fixtures are in disposable `temp/latency-auto/`.

Closed: NVFBC grabs on its own thread into one replaceable slot; render on arrival. Source age at grab 3-7 ms -> 0.15 ms at 120 Hz once the scanout modeset storm was removed

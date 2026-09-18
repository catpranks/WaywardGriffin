# Latency instrumentation: age at grab, commit-to-present, zero-copy flag, direct capture

Current stats aggregate everything into `present_ms` (NVFBC timestamp to compositor presentation). Cannot tell where the 30 ms goes. Need per-frame: age of the source frame at grab, grab-to-submit, time inside `queue_present`, submit-to-presented, frame-callback interval, presentation-feedback flags (ZERO_COPY means direct scanout), NVFBC `bDirectCapture`. Exposed through `stats` on the control socket.

Baseline 2026-09-18 (unlocked session, waygriff fullscreen on F1, Hyprland reports it as solitary, directScanoutTo 0, game at 120 FPS): render_hz 40, miss_hz 80, present_ms avg 30 / min 13.3 / max 72, wait+obtain+commit < 0.5 ms.


## Measurements, 2026-09-18 instrumentation/capture experiment

Fullscreen F1, HDMI-A-1 3840x2160@120, source :80 game menu showing 120 FPS.
Hyprland reported waygriff solitary, `directScanoutTo=0`; no hyprlock process.
NVFBC `bDirectCapture` was true for all measured displayed frames;
presentation flags were 7 (VSYNC | HW_CLOCK | HW_COMPLETION), ZERO_COPY fraction 0.
Presentation clock was CLOCK_MONOTONIC.

The old `present_ms` sampled feedback receipt, not the protocol's presentation
timestamp. The new total excludes feedback delivery delay. Below are means of
nine stats snapshots per run, sampled every 2 s at uptime 16–32 s; timing windows
contain up to 600 displayed frames and overlap. Rates use the last 2 s. These are
repeated observations, not independent statistical samples.

| Metric (ms except Hz) | Callback A | Callback B | Callback C | Thread, fresh A | Thread, fresh B |
|---|---:|---:|---:|---:|---:|
| render Hz | 49.64 | 53.74 | 57.64 | 54.17 | 38.56 |
| capture Hz | 49.63 | 53.75 | 57.64 | 120.06 | 120.06 |
| source age at grab | 1.370 | 3.873 | 0.946 | 0.158 | 0.159 |
| grab → submit | 0.400 | 0.406 | 0.412 | 0.423 | 0.412 |
| submit → presented | 18.211 | 17.156 | 16.053 | 15.280 | 19.464 |
| source → presented | 19.982 | 21.435 | 17.411 | 15.861 | 20.034 |
| feedback delivery | 1.718 | 1.744 | 1.582 | 1.227 | 1.962 |
| queue_present CPU | 0.028 | 0.027 | 0.028 | 0.025 | 0.028 |
| image acquire CPU | 0.009 | 0.009 | 0.009 | 0.030 | 0.009 |
| callback interval | 20.206 | 18.615 | 17.394 | 18.584 | 26.000 |
| render idle | 19.700 | 18.125 | 16.884 | 18.450 | 25.893 |

The first three latency rows add to source→presented. CUDA copy/buffer obtain
was 0.357–0.374 ms; fence waits about 0.001 ms; queue submission 0.024–0.027 ms.
Across callback runs the observed source→presentation extrema were 7.370–67.747 ms;
across fresh-thread runs 7.152–61.461 ms. Thread captures had no steady-state backend
misses, but 66–82 captured frames/s were dropped before submission.

Unrestricted frame-arrival rendering instead measured 51.922 ms total:
0.153 age + 19.511 grab→submit + 32.258 submit→presented, with 16.684 ms in
image acquisition. A one-outstanding-presentation variant consuming the latest
waiting frame measured 22.775 ms, including 7.136 ms grab→submit. Waiting for a
fresh frame after presentation produced the two final thread columns above;
its apparent first-run gain did not survive repetition. Capture-thread changes
were reverted; instrumentation remains. Raw samples and discarded experimental
patch: `temp/latency/` (disposable).

Final retained-instrumentation runtime check (callback architecture restored),
600-frame snapshot: render/capture 39.5 Hz; source age 5.457 ms + grab→submit
0.407 ms + submit→presented 19.710 ms = 25.574 ms; feedback delivery 2.366 ms;
queue_present 0.026 ms; callback interval 25.317 ms; render idle 24.843 ms.
This additional drift reinforces that a single before/after run does not
establish an end-to-end latency improvement. The additive identity was checked
against the JSON values. Final PNG was read and the instance stopped.


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

Closed: stats now break present_ms into age_at_grab, grab_to_submit, submit_to_present plus callback cadence, presentation flags, direct-capture and zero-copy fractions; docs/latency.md defines them

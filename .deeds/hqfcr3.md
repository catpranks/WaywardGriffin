# Try a higher in-game fps limiter (240 or off) with vsync off: expected ~2 ms mean latency win

Context: the game on :80 runs with vsync off and a 120 fps limiter (owner's
setting). The render loop takes the newest capture 5 ms before the predicted
AMD vblank, so the frame's age at that moment averages half the source period
plus the ~0.9 ms D2H copy: about 5 ms at 120 fps. At 240 fps that becomes about
3 ms; uncapped, lower still. This is a larger win than trimming the 5 ms
render-ahead (k9hved) and it also dissolves the source/output beat: with the
source much faster than the output, the phase at the deadline is effectively
random and the rolling mean sits still instead of sweeping 3.6-6.8 ms over
minutes (trace of 2026-09-18 at 120 fps vs 120.00 Hz, ~35 ppm drift).

Why not vsync on instead: it locks the game to the Nvidia HDMI-0 scanout
(119.88 Hz), which is just a different crystal against the AMD pixel clock,
same ppm regime, plus the game's swapchain queue latency. No benefit.

Trade-off: with the source faster than the fixed 120 Hz output, consecutive
displayed frames carry uneven game-time spacing (up to one source period of
jitter), i.e. classic vsync-off micro-stutter. At 240 fps that is up to 4 ms of
unevenness on an 8.3 ms cadence; uncapped it shrinks further. 120 fps against
120 Hz is the evenest animation short of VRR, at the cost of ~2 ms mean latency
and the slow slip. VRR (848742) removes the trade-off.

Costs to watch: RTX 5090 power and fan noise at 240+ fps on an old engine at
4K; NVFBC capture thread copying every grabbed frame (0.9 ms each, so fine to
several hundred fps); PCIe traffic 4 GB/s per 120 fps.

Procedure: owner sets the limiter in HoN on :80. Then
`waygriffctl start && waygriffctl set confine on`, sample `stats` every second
for 90 s, and compare `age_at_grab_ms + grab_to_submit_ms` (expect flat, ~3 ms)
and `present_ms` mean (expect ~8 ms) against the 120 fps baseline (drifting
3.6-6.8, mean 9-11). Also check `capture_drop_hz` rises as expected and that
`obtain_ms` stays ~0.9.

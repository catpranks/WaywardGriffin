# Recurring 25-45 ms present spikes in every 5 s window on the HoN menu

Observed 2026-09-18 on the SDMA + pacing build (4f18d633), confined, HoN at the
lobby/store menu, direct scanout engaged (`zero_copy_fraction` 1.0). Sampling
`waygriffctl stats` every second for 90 s, `present_max_ms` (max over the last
600 displayed frames, i.e. the last ~5 s) was never below 22 ms and usually
25-45 ms. So something stalls a frame by 3-5 refresh periods at least once
every 5 s, continuously. Mean present stayed 9-11 ms, so these are isolated
frames, not a cadence problem.

Unknown whether the stall is the source (menu animation or chat hitching, an
NVFBC grab that returns late), the capture thread, the render deadline path,
or the compositor. Candidates to separate them:

- Log per-frame `FrameLatency` for frames with total > 20 ms: which stage is
  long (age_at_grab, grab_to_submit, submit_to_present)?
- Compare against a source workload known to be steady (e.g. a simple 120 fps
  X test pattern on :80) to rule the game menu in or out.
- Check whether the 50 ms maintenance path fires (`has_pending_presentation`
  gate) by counting maintenance redraws.

Not a blocker: the owner reports play feels smooth. File findings here.

# Log

Append-only. Newest entry last.

## 2026-09-18 refresh

- Dependencies upgraded across the board (sctk 0.21 migration done by Codex, vello 0.10/wgpu 29, x11rb 0.14, cudarc 0.19 stayed).
- Added the control socket, `stats` module and headless monitor mode so an agent can run waygriff without the TUI. `waygriffctl` wraps it and places the window on Hyprland workspace F1 fullscreen via `hyprctl eval`.
- Layout pass: `src/capture` split into `render/`, `source/`, `bridge/`; shaders moved to `render/shaders.rs`; barrier and descriptor-set duplication folded into helpers; `plotter` renamed `monitor`; `GlobalStateExt::update` replaces the `with_*` builders; `memcpy-test` bin and the dummy bridge deleted; `vello` bin renamed `overlay-client`.
- Codex review of the control/launcher commit found seven issues (PID reuse, unacknowledged `set`, launcher exit codes, stop racing start, readiness, shell quoting, Nix paths); all fixed.
- Removed `docs/.obsidian`. Docs and AGENTS.md rewritten for the new layout.
- Baseline stats recorded in the cursor-latency issue for the coming optimization session: render ~50 Hz against a 120 Hz source, present ~27 ms.

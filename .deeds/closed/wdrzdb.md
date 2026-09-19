---
tier: objective
---
# TUI: readable chart, status block, structured log pane; waygriffctl start --tui

Review of the `--tdelay` dashboard against a live NVFBC run (4K@120, 180x50 and
100x30 terminals) found the display technically complete but hard to read:

- Chart y axis is `1.2 × max`, so one 30 ms spike squashes the 6 ms steady
  state into the bottom three rows. No reference line at one refresh. Frames
  without presentation feedback plot as 0 ms. One-entry legend and "ms" axis
  title overlap the plot; geometry lives in the chart title.
- Status line uses one-letter keys (`C` means three things), flags appear and
  disappear, and the health signals the docs say to verify (zero-copy fraction,
  feedback age, drop rate, window fill) are absent. Legacy W/O/C fields headline
  while the additive `age + submit + out` breakdown is not shown.
- Log pane has no level colour, no timestamps, wraps mid-token, resets scroll on
  every new line, shows no key help, and Ctrl-C is swallowed by raw mode.
- Snapshot publish and input polling follow `--tdelay`, so the control socket
  serves second-old stats in TUI mode.

Plan: expand the status to three labelled lines with fixed flag slots and
coloured alerts; axis in whole refresh intervals with outliers clipped at the
top; second series for submit→present; structured log events through a tracing
layer (relative time, level colour, word wrap, follow/scroll, repeat collapse);
fixed 100 ms poll/publish with `--tdelay` only controlling redraws.

Systemize the launch trick: `waygriffctl start --tui` runs waygriff inside a
detached tmux session on a project-local socket, exec'd by Hyprland so the
exec-rule token still places the window on F1; `waygriffctl tui` captures the
pane.

Closed: Implemented; see docs/log.md 2026-09-18 TUI review and rework. Not done: right-aligned wrap indent for continuation lines, and status lines still truncate below ~100 columns.

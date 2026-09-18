---
tier: objective
---
# Agent affordances: headless control socket, stats, waygriffctl

The TUI (`--tdelay`) is the only monitoring surface. Agents need something machine-readable instead.

Scope:
- A control socket next to the existing `@waygriff-0.snap` and `@waygriff-0.input` sockets, newline-delimited JSON. At minimum: the stats the TUI status line shows (render/capture rates, skip and miss rates, wait/obtain/commit/present timings, min/max present, state flags, source/window/render sizes), plus setting confine/capture/force-relative.
- A `waygriffctl` CLI that wraps the socket (and the screenshot socket).
- Headless mode (no `--tdelay`) prints logs to stdout; make sure that stays usable and consider a JSON log format flag.

The user's invocation is `cargo run --release --bin waygriff -- --backend=nvfbc --display=:80 --tdelay=1000ms`.

Closed: control socket @waygriff-0.ctl (stats/state/set/quit), --stats-every JSON log lines in headless mode, waygriffctl CLI

---
tier: objective
---
# Upgrade cargo dependencies

User asked for a full-send upgrade (`cargo upgrade --incompatible`, fix what breaks).

Incompatible bumps at time of filing: smithay-client-toolkit 0.20 -> 0.21, x11rb 0.13 -> 0.14, vello 0.7 -> 0.10, itertools 0.14 -> 0.15, pollster 0.4 -> 1.0, cc 1.2 -> 1.4. vulkano, smithay, cudarc have no newer major.

Done when `cargo build --release --all-targets` and `cargo clippy` are clean and the nvfbc path runs end to end against `:80`.

Closed: cargo upgrade --incompatible applied; sctk 0.21 migration to delegate_dispatch2 and Dispatch2 user-data markers; vello test client adapted to wgpu 29

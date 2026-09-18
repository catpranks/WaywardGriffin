---
tier: objective
---
# Code layout and quality pass

Chef's choice. Observations from the initial survey:

- `src/capture/mod.rs` (1200 lines) mixes Vulkan device setup, the render loop, three shaders, screenshot compute and the render thread entry point.
- `plotter.rs` couples stats aggregation with the ratatui UI; the log writer in `lib.rs` routes tracing output through it.
- `GlobalStateInner` has four `with_*` builders that are all `..self.clone()`.
- Frame acquire/release barriers in `Renderer::render` are four copies of the same barrier with different images.
- `src/bin/vello.rs` and `src/bin/memcpy-test.rs` are experiments; vello pulls wgpu/winit/pollster into the build.
- The Wayland capture backends are secondary; keep them building, no more.
- Keep the nvfbc path as the primary and verify it end to end after each change.

Closed: src/capture split into render/, source/, bridge/; shaders.rs; barrier + descriptor helpers; plotter -> monitor; GlobalStateExt::update replaces with_* builders; memcpy-test and dummy bridge removed; vello bin renamed overlay-client

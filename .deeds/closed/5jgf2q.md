# Presentation path: Nvidia Vulkan presenting to AMD Hyprland, direct scanout conditions

Investigated 2026-09-18. **First measure with direct scanout disabled; then pursue AMD rendering/presentation.** There is concrete evidence for a compositor-side problem before any assumption about slow cross-GPU copies: failed direct-scanout attempts enter a rollback path that marks the unchanged output format dirty, causing Aquamarine to use blocking modeset-style commits. The running compositor's log is flooded with the corresponding modeset messages. This is the strongest lead for the reported 40 Hz, but its contribution needs an A/B measurement.

Separately, AMD presentation is a defensible architecture. A matching Nvidia Vulkan probe produced linear, foreign DMA-BUFs that AMD could import for rendering but rejected as KMS framebuffers. The equivalent RADV buffers passed framebuffer creation. CUDA-to-RADV shared host memory also worked, with a measured 4K transfer cost around 0.60 ms. Neither direct scanout nor 120 Hz has been demonstrated for an AMD version of waygriff.

The setup under investigation is RTX 5090 / Nvidia 595.99.02 capturing X `:80`, fullscreen game, no X compositor, HDMI-0 at 3840×2160@119.88; destination Hyprland 0.56.2 on the Ryzen 9 9950X3D iGPU, HDMI-A-1 at 3840×2160@120, scale 1.25, workspace F1, `render:direct_scanout=1`. The reported application cadence is 40 Hz, with individual timed steps under 0.5 ms and capture-to-feedback latency averaging 30 ms, minimum 13.3 ms. Those timing figures are supplied observations, not measurements reproduced here. Passive `hyprctl monitors` confirmed the solitary candidate, null blockers, and `directScanoutTo: "0"`.

**Evidence boundaries.** Waygriff was not launched, stopped, traced, or reconfigured. Application source was read from committed revision `49bb84e6cb46895339390f25fe86b6f2e15e62c7`; concurrent working-tree changes are outside this analysis. The probes were separate short programs under `/tmp/waygriff-wsi.49JALs`, creating unroled, unmapped Wayland surfaces. Their buffer properties are directly measured; applying those properties to the live waygriff buffers is an inference from matching device, size, format, usage, and present mode. No atomic display commit or modeset was issued by a probe. Successful temporary framebuffer registrations were removed immediately.

**1. What Nvidia WSI gives AMD.** The Vulkan probe requested `B8G8R8A8_SRGB`, nonlinear-sRGB colorspace, 3840×2160, `COLOR_ATTACHMENT | TRANSFER_DST`, opaque composite alpha, Mailbox, and at least two images, matching the committed application settings. `WAYLAND_DEBUG=client`, DMA-BUF fdinfo, and DRM ioctls produced:

| Property | Nvidia WSI | AMD RADV WSI |
|---|---|---|
| Swapchain images returned | 2 | 4 |
| Wayland DRM format | `XR24` / XRGB8888, `0x34325258` | Same |
| Modifier | `0`, `DRM_FORMAT_MOD_LINEAR` | `0x0200000000401b03`, AMD tiled |
| Planes, offset, stride | 1, 0, 15,360 bytes | Same |
| Exported allocation size | 33,177,600 bytes | 33,423,360 bytes |
| Import into AMD using `drmPrimeFDToHandle` | Success | Success |
| AMD GEM preferred domains | `0x2`, GTT | `0x6`, GTT + VRAM |
| AMD GEM allocation flags | `0x200`; no `CPU_GTT_USWC` | `0x28e`; includes `CPU_GTT_USWC` |
| `drmModeAddFB2WithModifiers`, using each buffer's advertised layout | `-EINVAL` | Success |

The decisive Nvidia wire requests were:

```text
zwp_linux_buffer_params_v1.add(fd, 0, 0, 15360, 0, 0)
zwp_linux_buffer_params_v1.create_immed(wl_buffer, 3840, 2160, 875713112, 0)
wp_linux_drm_syncobj_surface_v1.set_acquire_point(timeline, 0, 1)
wp_linux_drm_syncobj_surface_v1.set_release_point(timeline, 0, 2)
wl_surface.attach(wl_buffer, 0, 0)
wl_surface.commit()
```

Opaque Vulkan composite alpha permits XRGB even though the application requested a Vulkan format containing alpha. On this little-endian machine the pixels have B,G,R,X byte order; the DRM fourcc does not encode Vulkan's sRGB transfer function.

The functioning cross-device path uses **system-memory-backed DMA-BUFs, mapped into AMD's GTT**, not an AMD-readable Nvidia optimal-tiled VRAM image. This conclusion combines the observed linear buffers/import with the driver source: strace showed Nvidia `GEM_IMPORT_NVKMS_MEMORY` followed by PRIME export; Nvidia 595.99.02 supplies the scatter/gather pages for that export from non-vidmem allocations, and its `prime_get_sg_table` fails when those pages are absent. Physical page residency and the proprietary Vulkan allocation/copy machinery were not independently instrumented. See [Nvidia's NVKMS GEM implementation](https://github.com/NVIDIA/open-gpu-kernel-modules/blob/595.99.02/kernel-open/nvidia-drm/nvidia-drm-gem-nvkms-memory.c#L298-L378).

Consequently, the original premise that “Nvidia buffers cannot be imported by AMD” is too broad. The tested Nvidia WSI export imports successfully. Sharing a compatible linear system-memory allocation is different from importing the application's Nvidia optimal image or using direct VRAM peer access.

A separate DMA-BUF feedback probe reported main device and tranche target `226:128`, `/dev/dri/renderD128`, the AMD iGPU. Its default rendering tranche had flags 0 and advertised these modifiers for both XR24 and AR24:

```text
0x020000000056bb03  0x0200000000401b03  0x0200000000401603
0x0200000000000901  0x0000000000000000  0x00ffffffffffffff
```

The final entry is `DRM_FORMAT_MOD_INVALID`, the implicit-modifier marker. This is a rendering/import capability list, not a promise that every allocation with one of these modifiers can scan out. The unmapped probe did not receive a scanout tranche. Hyprland's source constructs scanout tranches from the monitor plane formats but deliberately excludes LINEAR and INVALID from them. The application's own feedback handler merely stores the feedback; it does not use it to choose the Vulkan device. Nvidia WSI independently requests surface feedback, as observed on the wire. [Hyprland feedback construction](https://github.com/hyprwm/Hyprland/blob/efb50993780079460b0cbed1363e2166a2de1d9f/src/protocols/LinuxDMABUF.cpp#L28-L91).

**Copy count is partly established, not completely measured.** The committed code performs one synchronous CUDA device-to-array copy from the NVFBC capture into an Nvidia Vulkan image, then a full-frame application draw into the Nvidia swapchain. Hyprland imports the submitted DMA-BUF as a texture and composites it on AMD when scanout fails. No CPU pixel-copy fallback appears in that Hyprland import path. Aquamarine's separate multi-GPU-output blit is not the explanation here: the log identifies AMD as its sole selected/primary DRM device, and that blit is for secondary output backends.

The likely ordinary pipeline is:

```text
NVFBC device image
  → CUDA copy to Nvidia sampled image                 [application source]
  → Nvidia draw to swapchain                          [application source]
  → WSI transfer into linear system-memory DMA-BUF    [internal operation inferred]
  → AMD texture sampling/composition                  [Hyprland source]
  → AMD-owned scanout buffer
```

That model has two copy operations and two rendering passes after capture. The exact WSI copy count is unknown: the proprietary implementation may render into shareable memory or use an intermediate image and blit. Additional compositor passes also depend on its rendering configuration. What is unavoidable is the frame data crossing from the Nvidia side into host memory accessible to AMD; no evidence supports asserting multiple CPU copies. Thus “one WSI readback/blit” is a useful model, not a verified internal trace.

**Completion is explicitly synchronized.** In the probe, Nvidia WSI imports DRM syncobj timelines and attaches acquire and release points to each committed buffer. The application's Vulkan render-finished semaphore orders rendering before presentation work; WSI's acquire point must cover everything necessary to make the exported pixels readable, including any internal transfer. Hyprland waits for that point before promoting the pending surface state. Its composition completion fence is attached to the buffer's release machinery before WSI may reuse it. The application submission fence alone does not establish that the compositor has received, used, or released the WSI buffer. [Hyprland surface scheduling](https://github.com/hyprwm/Hyprland/blob/efb50993780079460b0cbed1363e2166a2de1d9f/src/protocols/core/Compositor.cpp#L570-L615), [composition release fences](https://github.com/hyprwm/Hyprland/blob/efb50993780079460b0cbed1363e2166a2de1d9f/src/render/GLRenderer.cpp#L109-L145).

This can delay a client's frame callbacks without blocking the entire compositor on that acquire fence: callbacks belong to the pending state and `frame()` sends callbacks from the current state. A late acquire signal delays eligibility. Forty client callbacks per second does not, by itself, prove that the compositor repaints the output at only 40 Hz. Mailbox and two frames in flight also do not establish a 40 Hz limit.

**The stronger pacing suspect is repeated blocking output reconfiguration.** The existing Hyprland log contains repeated sequences of:

```text
drm: Failed to submit a buffer with drmModeAddFB2WithModifiers
drm: Failed to submit a buffer to KMS
drm: Buffer failed to import to KMS
```

It also contains over 96,000 `Modesetting HDMI-A-1 with 3840x2160@120.00Hz` messages; one passive three-second sample appended 220 such messages. These entries lack client IDs and timestamps. Their count is evidence of a sustained reconfiguration path, not an exact measurement of refresh or application cadence, particularly while the live instance is being worked on independently.

The matching source explains a concrete mechanism:

1. `CMonitor::attemptDirectScanout()` installs a rollback scope guard. Every failure after that point calls `m_output->state->setFormat(previousFormat)`, even when the attempted buffer has exactly the same XRGB format as the output.
2. Aquamarine's `setFormat()` unconditionally sets `AQ_OUTPUT_STATE_FORMAT`. Its `needsReconfig()` tests this bit, rather than whether the format actually changed.
3. The following composited commit therefore takes `BLOCKING = NEEDS_RECONFIG || ...`, logs “Modesetting”, and sets `data.modeset`.
4. The atomic backend supplies `DRM_MODE_ATOMIC_ALLOW_MODESET` and omits `DRM_MODE_ATOMIC_NONBLOCK` for blocking commits.

```cpp
// Hyprland: failed-scanout rollback
m_output->state->setFormat(previousFormat);

// Aquamarine: even the same format marks a reconfiguration
internalState.drmFormat = drmFormat;
internalState.committed |= AQ_OUTPUT_STATE_FORMAT;
```

Aquamarine caches buffers rejected by framebuffer creation, so subsequent scanout tests can fail without repeating the AddFB error; Hyprland still runs the rollback. That explains why a few import failures can be followed by an enormous run of modeset messages. [Hyprland rollback](https://github.com/hyprwm/Hyprland/blob/efb50993780079460b0cbed1363e2166a2de1d9f/src/output/Monitor.cpp#L2153-L2182), [Aquamarine dirty-state handling](https://github.com/hyprwm/aquamarine/blob/1a10fe26a9f7d989c359e6a9ea61aa2e44d06c36/src/output/Output.cpp#L54-L107), [commit path](https://github.com/hyprwm/aquamarine/blob/1a10fe26a9f7d989c359e6a9ea61aa2e44d06c36/src/backend/drm/DRM.cpp#L2090-L2123), [atomic flags](https://github.com/hyprwm/aquamarine/blob/1a10fe26a9f7d989c359e6a9ea61aa2e44d06c36/src/backend/drm/impl/Atomic.cpp#L576-L593).

This is a source-verified route to blocking compositor work outside waygriff's timings. It does not prove that the HDMI link physically retrains on every message, nor that each blocking commit costs exactly three refresh periods. Attribution of the 40 Hz limit remains an inference pending the direct-scanout-off comparison.

The committed timing instrumentation has another limitation: `FrameInfo::mark_commit()` precedes `queue_submit` and `present`, so its commit interval excludes those calls and subsequent GPU/WSI work. `wp_presentation_feedback::Presented` ignores the supplied presentation timestamp and records `clock_monotonic_ns()` when the event is dispatched. The reported 30 ms therefore includes event delivery/dispatch delay; it is not a precise capture-to-scanout measurement. See committed `src/render/mod.rs:721`, `src/display/mod.rs:384`, and `src/stats.rs:32`. This needs accounting for when comparing backends.

**2. Why direct scanout fails despite null blockers.** `directScanoutBlockedBy` is calculated by `isDSBlocked(true)`: policy, candidate, geometry, transform, DMA-BUF availability, cursor/composition requirements, and color-management checks. It does not report subsequent framebuffer registration, atomic-test, or commit failures. `m_lastScanout`, which supplies `directScanoutTo`, is set only after success. The observed combination is therefore internally consistent. [Hyprland eligibility and attempt code](https://github.com/hyprwm/Hyprland/blob/efb50993780079460b0cbed1363e2166a2de1d9f/src/output/Monitor.cpp#L2016-L2223).

The probe establishes a failure earlier than the atomic test: AMD PRIME import succeeds, but AddFB fails. The matching Linux 7.2.6 source explains why. A foreign Nvidia import does not inherit `AMDGPU_GEM_CREATE_CPU_GTT_USWC`; the import code only preserves that flag from another AMD export. The measured Nvidia import flags were `0x200`, confirming its absence. `amdgpu_display_supported_domains()` permits GTT scanout only with that flag and the other platform prerequisites. Framebuffer creation rejects an imported buffer if GTT scanout is unavailable, because it cannot migrate that foreign allocation into AMD VRAM. [AMD DMA-BUF import](https://github.com/gregkh/linux/blob/v7.2.6/drivers/gpu/drm/amd/amdgpu/amdgpu_dma_buf.c#L415-L444), [scanout-domain conditions](https://github.com/gregkh/linux/blob/v7.2.6/drivers/gpu/drm/amd/amdgpu/amdgpu_display.c#L557-L578), [foreign-framebuffer rejection](https://github.com/gregkh/linux/blob/v7.2.6/drivers/gpu/drm/amd/amdgpu/amdgpu_display.c#L1300-L1337).

This is not simply an unsupported linear modifier. `drm_info` identifies HDMI-A-1's connector 393, CRTC 376, primary plane 206. Its XR24 `IN_FORMATS` includes LINEAR plus:

```text
0x0200000000533b03  0x0200000000563b03
0x0200000000537b03  0x0200000000567b03
0x0200000000401b03  0x0200000000401903
0x0200000000000901
```

The presently composited scanout framebuffer and the RADV probe both use `0x0200000000401b03`. The same 15,360-byte stride works for RADV. A supported format/modifier is necessary but insufficient: allocation provenance, memory-domain eligibility, valid pitches/offsets/size, and the eventual atomic plane/output test also matter. An AMD-native buffer can use the driver's appropriate placement; an imported foreign buffer has the additional GTT restrictions above.

For waygriff, the remaining compositor requirements include an opaque solitary surface, a 3840×2160 buffer matching the output's pixel size, matching transform, and no shader-only composition/color/cursor requirement. Scale 1.25 alone is not a rejection: the check compares actual buffer pixels with output pixels, and the live surface already passed it. The RADV AddFB successes establish plausibility, not a successful fullscreen atomic scanout test.

**3. Shape and measured cost of AMD rendering.** A workable path is:

```text
NVFBC CUDA device buffer
  → one CUDA D2H transfer into pinned host pages
  → RADV samples an imported linear image over those same pages
  → draw base frame, overlay and border into an AMD-native WSI image
  → Hyprland direct scanout if eligible; otherwise ordinary AMD composition
```

Allocate the host pages once per ring slot, pin with `cuMemHostRegister`, and import them into RADV with `VK_EXT_external_memory_host` / `HOST_ALLOCATION_BIT_EXT`. This allocation order avoids assuming that CUDA can register an arbitrary mapping returned by a Vulkan allocator. There is no CPU memcpy between CUDA's destination and RADV's input. The PCIe D2H transfer still exists: “zero-copy” here means shared host backing, not eliminating transport from the discrete GPU. [Vulkan host-memory import](https://docs.vulkan.org/refpages/latest/refpages/source/VK_EXT_external_memory_host.html), [CUDA memory API](https://docs.nvidia.com/cuda/cuda-driver-api/group__CUDA__MEM.html).

Verified on this machine:

- RADV, Mesa 26.2.2, advertises host-memory import with 4,096-byte pointer/allocation alignment. A CUDA-registered allocation imported successfully as RADV memory type 5 (`HOST_VISIBLE | HOST_COHERENT | HOST_CACHED`).
- A linear `B8G8R8A8_UNORM` image with `SAMPLED | TRANSFER_SRC` usage supports host-allocation import. Its queried row pitch was 15,360, offset 0, allocation size 33,177,600. Creating and binding that imported image succeeded. A real implementation must query these rather than hard-code them, preserve the intended sRGB handling, and account for source/destination pitches.
- CUDA filled those pages from device memory; a subsequent AMD Vulkan image-to-buffer copy reproduced every byte correctly. Thus shared backing and AMD access were exercised, not just extension enumeration. The fullscreen sampling shader itself was not benchmarked.

Short serialized benchmarks used a 33,177,600-byte device source, four warmups and 24 measured iterations per variant, CPU monotonic timing, and explicit completion waits. The desktop and live instance continued running; these are feasibility measurements, not isolated performance guarantees.

| Operation | Minimum | Median | 95th percentile |
|---|---:|---:|---:|
| CUDA D2H into the imported image's host pages, including stream synchronization | 0.588 ms | 0.595 ms | 0.796 ms |
| AMD imported-host buffer → AMD local mapped buffer, including submit/wait | 1.166 ms | 1.190 ms | 3.918 ms |
| AMD imported linear image → AMD local mapped buffer, including submit/wait | 2.117 ms | 2.260 ms | 5.319 ms |

The two AMD rows are alternative test operations, not cumulative required steps. The proposed direct-sampling path replaces that test copy with the application's draw. A simpler staged implementation can copy the shared buffer into an optimal AMD image before drawing, paying an additional local transfer.

3840×2160 BGRA is 31.640625 MiB/frame and 3.981 GB/s at 120 Hz. Nvidia reported PCIe Gen5 ×16; the measured D2H median corresponds to about 55.8 GB/s. Transport therefore plausibly fits comfortably within the 8.333 ms refresh budget. A few milliseconds for transfer plus AMD processing is credible; a measured 120 Hz complete renderer is still required, especially given the tails above. It is not defensible to promise a sub-0.5-ms complete AMD path.

Synchronization remains necessary even on coherent host memory. Wait for CUDA's D2H completion before submitting AMD reads; use the appropriate Vulkan visibility/layout barriers and retain the backing pages. Do not reuse a slot until AMD's last read has completed. A small ring plus completion fences is sufficient; CUDA stream synchronization is a valid initial implementation and was included in the measured transfer. The NVFBC source must also remain valid until D2H completes. No cross-vendor CUDA/Vulkan external semaphore compatibility was established or required for this test.

AMD presentation removes the foreign-buffer scanout rejection and the hidden Nvidia WSI presentation transfer from the design. It can also remove the current Nvidia-local capture copy and render pass. It does not guarantee lower latency if callback scheduling remains pathological or if the AMD renderer misses deadlines. For scanout, the final buffer should remain an ordinary AMD-native WSI allocation; the imported host capture image is the input texture, not the proposed scanout buffer.

**Measurements that settle the remaining decisions.** The live-instance owner should first compare identical workloads with `render:direct_scanout` at 1 and 0, confirming whether the modeset log flood stops and recording callback intervals, true presentation timestamps, and latency. This isolates failed-scanout fallback while retaining Nvidia rendering and its WSI transport. It is a more discriminating first test than replacing the whole renderer. Restore the original setting after the comparison. No such configuration change was made during this investigation.

If that comparison restores the cadence, fix/work around the failed-scanout rollback marking unchanged formats dirty; do not attribute the original 40 Hz to PCIe bandwidth. AMD rendering is still attractive for scanout and explicit control of the transfer. If the cadence persists, trace one correlated sequence: Vulkan render completion, WSI `wl_surface.commit`, acquire-point signal/Hyprland state promotion, compositor render and atomic-commit completion, presentation timestamp, and frame-callback delivery. `WAYLAND_DEBUG` alone cannot time GPU completion. Logging acquire wait durations and DRM commit durations distinguishes a late buffer from a blocked compositor. A fullscreen AMD prototype should then verify `directScanoutTo`, presentation feedback flags, frame rate and latency, rather than treating AddFB success as the final scanout proof.

Source provenance: Hyprland checkout was clean at running-build commit `efb50993780079460b0cbed1363e2166a2de1d9f` (v0.56.2). The existing Aquamarine checkout was older, so analysis used `1a10fe26a9f7d989c359e6a9ea61aa2e44d06c36`, pinned by that Hyprland flake, matching its reported Aquamarine 0.15.0 build dependency. Kernel comparisons used upstream v7.2.6, matching the running kernel version, plus measured GEM flags and ioctl results. Nvidia kernel source was tag 595.99.02. Mesa's local checkout was incomplete; RADV conclusions above come from the installed driver's queries/probes, not assumed Mesa WSI internals. wlroots is not the compositor implementation used here.

## A/B confirmed 2026-09-18 (Claude)

Runtime toggle via `hyprctl eval 'hl.config({render={direct_scanout=N}})'`, waygriff fullscreen on F1, game at 120 FPS, 600-frame windows, modesets counted over 12 s in the Hyprland log:

| direct_scanout | render Hz | callback interval | present avg / min / max ms | age_at_grab | submit_to_present | modesets / 12 s |
|---|---:|---:|---:|---:|---:|---:|
| 1 (enable) | 47-55 | 18-21 ms | 22.3 / 10.5 / 69.2 and 18.6 / 9.9 / 68.6 | 3.1-3.6 | 15.1-18.3 | 660-770 |
| 0 (disable) | 118-120 | 8.33 ms | 19.8 / 18.9 / 28.0 and 23.4 / 15.4 / 23.8 | 4.0-7.3 | 14.9-15.4 | 0 |
| 2 (auto) | 109-120 | 8.32 ms | 22.1 / 15.1 / 23.8 | 6.3 | 14.7 | 0 |

The modeset storm is the cause of the sub-60 Hz loop. With it gone, the loop runs at the display rate and the remaining cost is `submit_to_present` of about two refresh periods (composited Nvidia system-memory buffer) plus half a period of callback pacing.

`auto` (2) blocks scanout for surfaces without content type `game`; `directScanoutBlockedBy: content type` is reported. waygriff sets no content type, so `auto` is the per-window opt-out: native games that set content-type still get scanout. When waygriff presents AMD-native buffers it can set content type `game` to opt back in.

Upstream bug worth reporting (not filed): `CMonitor::attemptDirectScanout` rollback calls `setFormat(previousFormat)` even when the format did not change; aquamarine's `setFormat` sets `AQ_OUTPUT_STATE_FORMAT` unconditionally, so every failed attempt forces a blocking modeset.

Closed: answered: Nvidia WSI exports linear sysmem DMA-BUFs AMD can composite but not scan out; failed scanout attempts force blocking modesets; direct_scanout=2 is the opt-out. AMD-side rendering continues as 8kxgv6

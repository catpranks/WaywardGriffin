---
tier: nit
---
# Border effect draws a full-viewport blended triangle; restrict to the edge strips
`fs_border` runs on every pixel of the 4K viewport with alpha blending and a branch, then discards everything outside a 5-25 px strip. Cheap on the 5090, potentially material on the AMD iGPU once rendering moves there (8kxgv6). Draw four edge quads or set a scissor per edge instead. Also: every latency measurement before 2026-09-18 evening was taken unconfined, so the border was active; compare confined vs unconfined when judging the AMD path.

Closed: Measured full-viewport border at roughly 0.5–0.7 ms on RADV; four disjoint 25-pixel scissors reduce the isolated border pass to about 0.03–0.04 ms. Confined headline measurements and paired unconfined checks are documented.

# RustSearch GPU Port Plan

## Goal

Replace current CPU raster path (`egui_ratatui` + `soft_ratatui`) with a GPU-native rendering path while preserving command/search/indexing behavior and existing keyboard UX.

## Why

- Current pipeline rasterizes TUI frames on CPU, then uploads textures to GPU.
- Full-height/fullscreen increases software raster workload and texture upload cost.
- A GPU-native path should reduce frame time spikes and improve responsiveness at large window sizes.

## Phased Migration

1. Preserve backend logic, replace frontend rendering only.
2. Introduce a new `gpu_ui` module that draws panel/results/progress directly in egui.
3. Keep existing `AppState` and command/index/search pipelines intact.
4. Add a runtime toggle/env flag to compare old vs new renderer during transition.
5. Remove `egui_ratatui`/`soft_ratatui` path once parity and performance are validated.

## Initial Tasks

- [ ] Add renderer abstraction (`UiRenderer`) with `render(ctx, app_state)`.
- [ ] Implement first GPU renderer skeleton using egui widgets/canvas.
- [ ] Port prompt + results list + status/footer.
- [ ] Port command popup and notice overlays.
- [ ] Port progress bar with same phase labels.
- [ ] Add simple frame-time debug readout for baseline comparison.

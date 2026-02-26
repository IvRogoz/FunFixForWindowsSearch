# RustSearch

RustSearch is a `wizmini-native` rebuild using `eframe` + `egui_ratatui` + `ratatui`.

## Run (dev)

```bash
cargo run --manifest-path RustSearch/Cargo.toml
```

Renderer mode (optional):

- Default: GPU-native egui renderer
- Legacy soft ratatui renderer: set `RUSTSEARCH_RENDERER=soft`

## Build (release)

```bash
cargo build --release --manifest-path RustSearch/Cargo.toml
```

## Controls

- Backtick: show/hide panel (global hotkey)
- Type to search
- Arrow Up/Down, Page Up/Down, Home/End to navigate
- Enter open file
- Alt+Enter reveal in Explorer
- Esc hide panel
- Slash commands: `/entire`, `/all`, `/x:`, `/up`, `/track`, `/latest`, `/last`, `/reindex`, `/testProgress`, `/about`, `/exit`
- Renderer commands: `/gpu` (GPU-native UI) and `/soft` (legacy soft-raster ratatui UI)

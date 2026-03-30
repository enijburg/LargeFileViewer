# LargeFileViewer (Rust, Windows x64 friendly)

A terminal-based large file viewer implemented in Rust that uses a **memory-mapped file** so it can inspect huge files without loading them fully into process heap memory.

## Why this design

- Uses `memmap2` to map the file into virtual memory.
- Builds a compact line-offset index (`Vec<usize>`) once.
- Renders **only the currently visible lines** in the terminal viewport.
- Scrolling updates just the visible window.

This pattern is ideal for large logs, CSVs, and text dumps.

## Build

```bash
cargo build --release --target x86_64-pc-windows-msvc
```

If you are already on Windows x64 with Rust MSVC toolchain installed, this generates:

`target/x86_64-pc-windows-msvc/release/lfv.exe`

## Run

```bash
cargo run -- path/to/very_large_file.log
```

Useful options:

- `--csv`: align comma-separated fields into columns.
- `--xml`: enable rudimentary XML syntax highlighting.
- `--format`: pretty-prints `--xml` or `--json` input into indented lines.
- `--json`: enable rudimentary JSON syntax highlighting.
- `--tab-width N`: set visual tab width (default: 4).

### Controls

- `q`: quit
- `↑ / ↓`: scroll one line
- `PageUp / PageDown`: scroll by one terminal page
- `Home / End`: jump to start/end


## GUI editor

The project now also includes a desktop GUI editor (`lfv-gui`) built on the same core principles:

- Memory-map the file with `memmap2` (no full file load).
- Build a line-offset index once.
- Render only visible rows with egui virtualization (`ScrollArea::show_rows`).
- Use the `wgpu` renderer backend (no OpenGL requirement).
- Keep edits as sparse, in-memory per-line overrides until you save.

Run it with:

```bash
cargo run --bin lfv-gui -- path/to/very_large_file.log
```

GUI usage:

- Double-click a line to edit it.
- Press `Enter` to apply an edit for that line.
- Provide a destination path in `Save as` and click `Save` to write an edited copy.

## Notes

- Best with text files (UTF-8 or ASCII-like content).
- Non-printable bytes are shown as `·`.
- Tabs render as spaces (default width 4, configurable with `--tab-width`).

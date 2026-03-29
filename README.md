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

## Notes

- Best with text files (UTF-8 or ASCII-like content).
- Non-printable bytes are shown as `·`.
- Tabs render as spaces (default width 4, configurable with `--tab-width`).

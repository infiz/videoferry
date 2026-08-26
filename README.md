# VideoFerry

VideoFerry is a friendly, open-source desktop video converter for Windows and
macOS. It uses Rust, Slint, and FFmpeg's native libraries directly: conversion
runs inside the application process rather than launching `ffmpeg` or
`ffprobe` subprocesses.

> VideoFerry is under active development. Keep backups of important media and
> test releases with non-critical files before relying on them in production.

## Highlights

- Native FFmpeg probing, transcoding, remuxing, trimming, stabilization, and
  slideshow pipelines.
- A responsive Slint interface with a persistent, reorderable task queue.
- Software, NVIDIA NVENC, and Apple VideoToolbox encoder support when the
  required hardware and runtime capabilities are available.
- Live conversion progress, optional frame preview, pause/resume controls, and
  system-sleep prevention while work is active.
- Unicode filenames and bundled pan-CJK font coverage.
- Independent application state under the VideoFerry application-data folder.

## Repository layout

```text
assets/                  Runtime assets shared by platform packages
crates/app/              Slint desktop application
crates/converter-core/   Safe queue, settings, and conversion-domain logic
crates/ffmpeg-bridge/    Native FFmpeg integration and the only FFI boundary
crates/presets/          Workflow presets and validation
docs/                    Architecture, parity, and maintenance documentation
packaging/               Windows and macOS release packaging
testing/                 Cross-platform runtime and parity checks
```

## Development

The repository pins Rust 1.98.0. Basic workspace tests do not require a local
FFmpeg SDK:

```text
cargo test --workspace --locked
cargo run -p videoferry-app
```

Install the repository's Git checks once, then run them against the full tree:

```text
pre-commit install
pre-commit run --all-files
```

The hooks run `cargo fmt` and strict Clippy checks whenever relevant Rust or
workspace configuration files are committed.

Native FFmpeg builds require `FFMPEG_DIR` to point to a compatible shared
FFmpeg SDK and `LIBCLANG_PATH` to point to libclang:

```text
cargo run -p videoferry-app --features native-ffmpeg
```

No crate in this workspace may invoke `ffmpeg` or `ffprobe` as a subprocess.
See [the development status](docs/DEVELOPMENT_STATUS.md),
[implementation plan](docs/IMPLEMENTATION_PLAN.md), and
[FFmpeg upgrade guide](docs/UPGRADING_FFMPEG.md) for details.

The optional Windows parity matrix compares VideoFerry with the legacy Python
implementation. Pass its location with `-ReferencePythonProject` or the
`VIDEOFERRY_REFERENCE_PYTHON_PROJECT` environment variable when it is not in
the default sibling checkout location.

## Packaging

Windows release builds use `packaging/windows/build.ps1`. macOS Apple Silicon
release builds use `packaging/macos/build.sh`. Both package the pinned FFmpeg
shared libraries with VideoFerry and verify the native runtime before creating
release artifacts.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) before opening a change. Bug reports and
focused pull requests are welcome.

## License

VideoFerry is licensed under the
[GNU General Public License v3.0 or later](LICENSE). Packaged releases also
include FFmpeg and other third-party components under their respective
licenses.

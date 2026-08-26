# Contributing to VideoFerry

Thank you for helping improve VideoFerry.

## Before submitting a change

1. Keep changes focused and avoid committing generated media, SDKs, or build
   output.
2. Run `cargo fmt --all --check`.
3. Run `cargo clippy --workspace --all-targets -- -D warnings`.
4. Run `cargo test --workspace --locked`.
5. For native conversion changes, describe the sample input and expected
   output and run the relevant platform parity checks when possible.

Unsafe Rust belongs only in `crates/ffmpeg-bridge`. Other crates should remain
safe and must not launch `ffmpeg` or `ffprobe` subprocesses.

## Reporting problems

Include your operating system, VideoFerry version, FFmpeg runtime version,
encoder, workflow, and the smallest reproducible input description. Do not
upload private media or logs containing sensitive filesystem paths.

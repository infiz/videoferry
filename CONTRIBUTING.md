# Contributing to VideoFerry

Thank you for helping improve VideoFerry.

## Before submitting a change

1. Keep changes focused and avoid committing generated media, SDKs, or build
   output.
2. Install the repository hooks once with `pre-commit install`.
3. Run `pre-commit run --all-files` before committing. This checks Rust
   formatting and runs Clippy with warnings treated as errors.
4. If `pre-commit` is unavailable, run `cargo fmt --all -- --check` and
   `cargo clippy --workspace --all-targets --locked -- -D warnings` directly.
5. Run `cargo test --workspace --locked`.
6. For native conversion changes, describe the sample input and expected
   output and run the relevant platform parity checks when possible.

Unsafe Rust belongs only in `crates/ffmpeg-bridge`. Other crates should remain
safe and must not launch `ffmpeg` or `ffprobe` subprocesses.

## Reporting problems

Include your operating system, VideoFerry version, FFmpeg runtime version,
encoder, workflow, and the smallest reproducible input description. Do not
upload private media or logs containing sensitive filesystem paths.

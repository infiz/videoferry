# Upgrading the native FFmpeg engine

FFmpeg is part of the application release, not a separately replaceable user
tool. A DLL or dylib update can change ABI, codec behavior, licensing, and
output compatibility, so an engine upgrade is tested and shipped with a new
VideoFerry build.

## Upgrade procedure

1. Select one FFmpeg release and one compatible `ffmpeg-next` release. Do not
   use an unversioned archive in a release build.
2. Update and run the pinned platform SDK recipes. On Windows,
   `packaging/windows/install-ffmpeg-sdk.ps1` reads the URL, archive name,
   checksum, and install location from `engine-manifest.toml`, verifies the
   archive before extraction, and refuses to replace an existing SDK. On Apple
   Silicon, `packaging/macos/build-ffmpeg-sdk.sh` verifies the source archive
   checksum, records exact Homebrew dependency versions, and builds the
   required shared codecs, filters, and hardware APIs.
3. Record the source commit, binding version, library versions, package URLs,
   SHA-256 hashes, license, and local layout in `engine-manifest.toml`.
4. Update the exact `ffmpeg-next` version in the workspace manifest, the
   initialization major guards, and the packaged-runtime exact version guards
   together.
5. Update `rust-toolchain.toml` only when the release intentionally upgrades
   Rust. Both platform package scripts reject a compiler that does not exactly
   match that pin. Build the `native-ffmpeg` feature from clean Windows and
   macOS workspaces.
6. Run unit tests, strict Clippy, normalized probe comparisons, packet remux,
   software encoder, NVENC, VideoToolbox, subtitle/audio mapping, malformed
   input, pause/cancel, output validation, and packaging tests.
7. Bundle the validated DLLs or dylibs with the signed application. Require the
   packaged GUI binary's `--verify-runtime` result to pass outside development
   SDK paths; on macOS also run `packaging/macos/verify-package.sh` against the
   bundle, extracted ZIP, and mounted DMG. Never load an arbitrary `ffmpeg`
   installation from `PATH` in a packaged build. Windows release builds set the
   three `VIDEOFERRY_WINDOWS_SIGNING_*` values documented in `README.md`; the
   package builder verifies the resulting application and installer signatures,
   its static Rust CRT, and the exact six-DLL FFmpeg dependency closure.
8. Keep the previous signed application release available for rollback.

During development, `FFMPEG_DIR` points at the extracted SDK and
`LIBCLANG_PATH` points at `libclang`. End users will not configure either
variable; the installer/app bundle carries the exact libraries listed in the
manifest.

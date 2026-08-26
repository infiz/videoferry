#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
workspace_root="$(cd "$script_dir/.." && pwd)"
default_ffmpeg_dir="$workspace_root/.local/ffmpeg/macos-aarch64"
skip_ffmpeg_sdk_build=0

usage() {
    cat <<'EOF'
Usage: ./scripts/build_mac.sh [--skip-ffmpeg-sdk-build]

Build the Apple Silicon macOS application bundle, portable ZIP, and installable
DMG. When the default pinned FFmpeg SDK is missing, it is built first.

Options:
  --skip-ffmpeg-sdk-build  Fail instead of building a missing FFmpeg SDK.
  -h, --help               Show this help message.

Environment:
  FFMPEG_DIR                    Use an existing compatible FFmpeg SDK.
  LIBCLANG_PATH                 Directory containing libclang.dylib.
  VIDEOFERRY_CODESIGN_IDENTITY  Developer ID Application signing identity.
  VIDEOFERRY_NOTARY_PROFILE     notarytool keychain profile to submit the DMG.
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --skip-ffmpeg-sdk-build)
            skip_ffmpeg_sdk_build=1
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "Unknown option: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
    shift
done

if [[ "$(uname -s)" != "Darwin" ]]; then
    echo "The macOS installation package must be built on macOS." >&2
    exit 1
fi
if [[ "$(uname -m)" != "arm64" ]]; then
    echo "The macOS installation package currently targets Apple Silicon (arm64)." >&2
    exit 1
fi

ffmpeg_dir="${FFMPEG_DIR:-$default_ffmpeg_dir}"

ffmpeg_sdk_is_complete() {
    local required
    for required in \
        include \
        lib \
        LICENSE \
        BUILD-INFO.txt \
        licenses \
        lib/libavcodec.63.dylib \
        lib/libavfilter.12.dylib \
        lib/libavformat.63.dylib \
        lib/libavutil.61.dylib \
        lib/libswresample.7.dylib \
        lib/libswscale.10.dylib; do
        if [[ ! -e "$ffmpeg_dir/$required" ]]; then
            return 1
        fi
    done
}

if ! ffmpeg_sdk_is_complete; then
    if [[ -n "${FFMPEG_DIR:-}" ]]; then
        echo "The FFmpeg SDK configured by FFMPEG_DIR is incomplete: $ffmpeg_dir" >&2
        echo "Unset FFMPEG_DIR to build the pinned SDK in the default location." >&2
        exit 1
    fi
    if [[ "$skip_ffmpeg_sdk_build" == "1" ]]; then
        echo "The pinned FFmpeg SDK is missing or incomplete: $ffmpeg_dir" >&2
        echo "Run without --skip-ffmpeg-sdk-build to build it automatically." >&2
        exit 1
    fi

    echo "Building the pinned FFmpeg SDK..."
    bash "$workspace_root/packaging/macos/build-ffmpeg-sdk.sh"
fi

echo "Building the macOS application bundle and installation package..."
bash "$workspace_root/packaging/macos/build.sh"

app_version="$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$workspace_root/Cargo.toml" | head -n 1)"
dmg="$workspace_root/dist/macos/VideoFerry-$app_version-macos-aarch64.dmg"
if [[ -z "$app_version" || ! -f "$dmg" ]]; then
    echo "The build completed without producing the expected macOS installation package: $dmg" >&2
    exit 1
fi

echo "macOS installation package: $dmg"

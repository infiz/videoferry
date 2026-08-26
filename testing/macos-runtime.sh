#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
workspace_root="$(cd "$script_dir/.." && pwd)"

if [[ "$(uname -s)" != "Darwin" || "$(uname -m)" != "arm64" ]]; then
    echo "The native macOS media matrix requires Apple Silicon macOS." >&2
    exit 1
fi

ffmpeg_dir="${FFMPEG_DIR:-$workspace_root/.local/ffmpeg/macos-aarch64}"
app_bundle="$workspace_root/dist/macos/VideoFerry.app"
app_binary="$app_bundle/Contents/MacOS/VideoFerry"
source_base64="$workspace_root/testing/assets/synthetic-smoke-source.mp4.base64"
expected_source_sha256="4a1a967b4dc9c1417c5cdd6e7aedc67ffc3c0667eef08e27b815369bbf3b6fe9"

if [[ ! -x "$app_binary" ]]; then
    echo "Build the macOS package before running this matrix: bash packaging/macos/build.sh" >&2
    exit 1
fi
if [[ ! -f "$source_base64" ]]; then
    echo "Synthetic media fixture is missing: $source_base64" >&2
    exit 1
fi
for required in include lib; do
    if [[ ! -d "$ffmpeg_dir/$required" ]]; then
        echo "FFmpeg SDK is missing '$required': $ffmpeg_dir" >&2
        exit 1
    fi
done

libclang_path="${LIBCLANG_PATH:-}"
if [[ -z "$libclang_path" ]]; then
    developer_dir="$(xcode-select -p)"
    for candidate in \
        "$developer_dir/Toolchains/XcodeDefault.xctoolchain/usr/lib" \
        "$developer_dir/usr/lib"; do
        if [[ -f "$candidate/libclang.dylib" ]]; then
            libclang_path="$candidate"
            break
        fi
    done
fi
if [[ -z "$libclang_path" || ! -f "$libclang_path/libclang.dylib" ]]; then
    echo "libclang.dylib was not found; set LIBCLANG_PATH to its directory." >&2
    exit 1
fi

run_parent="$workspace_root/.local/macos-runtime-runs"
mkdir -p "$run_parent"
run_root="$(mktemp -d "$run_parent/run.XXXXXX")"
keep_artifacts="${VIDEOFERRY_KEEP_MACOS_RUNTIME_ARTIFACTS:-0}"
cleanup() {
    if [[ "$keep_artifacts" == "1" ]]; then
        echo "Native macOS media artifacts: $run_root"
    elif [[ -d "$run_root" && "$(dirname "$run_root")" == "$run_parent" && "$(basename "$run_root")" == run.* ]]; then
        rm -rf -- "$run_root"
    fi
}
trap cleanup EXIT

export FFMPEG_DIR="$ffmpeg_dir"
export LIBCLANG_PATH="$libclang_path"
export DYLD_LIBRARY_PATH="$ffmpeg_dir/lib${DYLD_LIBRARY_PATH:+:$DYLD_LIBRARY_PATH}"

cd "$workspace_root"
bash packaging/macos/verify-package.sh
cargo build --locked --release -p videoferry-ffmpeg \
    --features native-ffmpeg --example native_convert --example probe

converter="$workspace_root/target/release/examples/native_convert"
probe="$workspace_root/target/release/examples/probe"
source="$run_root/source.mp4"
base64 -D -i "$source_base64" -o "$source"
actual_source_sha256="$(shasum -a 256 "$source" | awk '{print $1}')"
if [[ "$actual_source_sha256" != "$expected_source_sha256" ]]; then
    echo "Synthetic source checksum mismatch: $actual_source_sha256" >&2
    exit 1
fi

photos="$run_root/photos"
mkdir -p "$photos"
photo_base64='iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII='
printf '%s' "$photo_base64" | base64 -D > "$photos/photo-1.png"
cp "$photos/photo-1.png" "$photos/photo-2.png"

case_count=0
hardware_case_count=0

expected_codec() {
    case "$1" in
        x264|h264_videotoolbox) printf '%s\n' h264 ;;
        x265|hevc_videotoolbox) printf '%s\n' hevc ;;
        svtav1|av1_videotoolbox) printf '%s\n' av1 ;;
        *) echo "Unknown encoder: $1" >&2; exit 1 ;;
    esac
}

run_case() {
    local name="$1"
    local mode="$2"
    local encoder="$3"
    local input="$4"
    local extension="$5"
    shift 5
    local output="$run_root/$name.$extension"
    local event_log="$run_root/$name.events.log"
    local probe_log="$run_root/$name.probe.log"
    local codec
    codec="$(expected_codec "$encoder")"

    (
        export VIDEOFERRY_MODE="$mode"
        export VIDEOFERRY_ENCODER="$encoder"
        if [[ "$mode" == "slideshow" ]]; then
            export VIDEOFERRY_SLIDESHOW="1280x720"
            export VIDEOFERRY_SLIDESHOW_FPS="12"
            export VIDEOFERRY_SLIDESHOW_INTERVAL="0.5"
        else
            unset VIDEOFERRY_SLIDESHOW VIDEOFERRY_SLIDESHOW_FPS VIDEOFERRY_SLIDESHOW_INTERVAL
        fi
        if [[ "$mode" == "stabilize" ]]; then
            export VIDEOFERRY_STABILIZE="Balanced"
        else
            unset VIDEOFERRY_STABILIZE
        fi
        "$converter" "$input" "$output" "$@"
    ) 2> "$event_log"

    if [[ ! -s "$output" ]]; then
        echo "$name produced no output." >&2
        exit 1
    fi
    "$probe" "$output" > "$probe_log"
    grep -qE "^PrimaryVideo: codec=$codec width=[1-9][0-9]* height=[1-9][0-9]* duration_ms=[1-9][0-9]*$" "$probe_log"
    grep -q 'Completed' "$event_log"
    case_count=$((case_count + 1))
    printf 'passed: %-34s %s\n' "$name" "$codec"
}

run_case tv-x265 tv x265 "$source" mkv
run_case animation-x265 animation x265 "$source" mkv
run_case camera-x265 camera x265 "$source" mp4
run_case stabilize-x265 stabilize x265 "$source" mp4
run_case trim trim x264 "$source" mp4 0 1
run_case slideshow-x265 slideshow x265 "$photos" mp4
hardware_source="$run_root/slideshow-x265.mp4"

runtime_report="$("$app_binary" --verify-runtime)"
available_encoders="$(awk -F= '$1 == "available_encoders" { print $2 }' <<<"$runtime_report")"
for required_encoder in h264_videotoolbox hevc_videotoolbox; do
    if ! grep -qE "(^|,)$required_encoder(,|$)" <<<"$available_encoders"; then
        echo "Required Apple Silicon encoder was not advertised: $required_encoder" >&2
        exit 1
    fi
done
for encoder in h264_videotoolbox hevc_videotoolbox av1_videotoolbox; do
    if ! grep -qE "(^|,)$encoder(,|$)" <<<"$available_encoders"; then
        echo "skipped unsupported hardware encoder: $encoder"
        continue
    fi
    short_name="${encoder%_videotoolbox}"
    for mode in tv camera slideshow stabilize; do
        input="$hardware_source"
        extension=mp4
        if [[ "$mode" == "slideshow" ]]; then
            input="$photos"
        elif [[ "$mode" == "tv" ]]; then
            extension=mkv
        fi
        run_case "$mode-$short_name-videotoolbox" "$mode" "$encoder" "$input" "$extension"
        hardware_case_count=$((hardware_case_count + 1))
    done
done

echo "Native macOS workflow gates passed: $case_count"
echo "VideoToolbox gates passed: $hardware_case_count"

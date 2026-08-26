#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
workspace_root="$(cd "$script_dir/../.." && pwd)"

if [[ "$(uname -s)" != "Darwin" || "$(uname -m)" != "arm64" ]]; then
    echo "The macOS package verifier requires Apple Silicon macOS." >&2
    exit 1
fi

app_bundle="${1:-$workspace_root/dist/macos/VideoFerry.app}"
expected_version="$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$workspace_root/Cargo.toml" | head -n 1)"
archive="${2:-$workspace_root/dist/macos/VideoFerry-$expected_version-macos-aarch64.zip}"
dmg="${3:-$workspace_root/dist/macos/VideoFerry-$expected_version-macos-aarch64.dmg}"

for command in codesign ditto hdiutil lipo otool plutil; do
    if ! command -v "$command" >/dev/null 2>&1; then
        echo "Required macOS verification tool is missing: $command" >&2
        exit 1
    fi
done
if [[ -z "$expected_version" ]]; then
    echo "Unable to read the workspace application version." >&2
    exit 1
fi

temporary_root="$(mktemp -d -t videoferry-macos-verify)"
mounted=0
mount_point="$temporary_root/dmg"

cleanup() {
    if [[ "$mounted" == "1" ]]; then
        hdiutil detach "$mount_point" -quiet || true
    fi
    if [[ -d "$temporary_root" && "$(basename "$temporary_root")" == videoferry-macos-verify.* ]]; then
        rm -rf -- "$temporary_root"
    fi
}
trap cleanup EXIT

assert_equal() {
    local actual="$1"
    local expected="$2"
    local message="$3"
    if [[ "$actual" != "$expected" ]]; then
        echo "$message (expected '$expected', got '$actual')" >&2
        exit 1
    fi
}

verify_architecture() {
    local binary="$1"
    local architectures
    architectures="$(lipo -archs "$binary")"
    assert_equal "$architectures" "arm64" "Unexpected architecture for $binary"
}

verify_dependencies() {
    local bundle="$1"
    local frameworks="$bundle/Contents/Frameworks"
    local binary dependency dependency_name
    while IFS= read -r binary; do
        while IFS= read -r dependency; do
            case "$dependency" in
                /System/*|/usr/lib/*) ;;
                @rpath/*)
                    dependency_name="$(basename "$dependency")"
                    if [[ ! -e "$frameworks/$dependency_name" ]]; then
                        echo "Missing bundled dependency for $binary: $dependency" >&2
                        exit 1
                    fi
                    ;;
                *)
                    echo "Unbundled dependency remains in $binary: $dependency" >&2
                    exit 1
                    ;;
            esac
        done < <(otool -L "$binary" | tail -n +2 | awk '{print $1}')
    done < <(
        printf '%s\n' "$bundle/Contents/MacOS/VideoFerry"
        find "$frameworks" -maxdepth 1 -type f -name '*.dylib' -print
    )
}

verify_runtime() {
    local binary="$1"
    local report
    report="$("$binary" --verify-runtime)"
    if [[ "$report" != runtime=ok$'\n'* ]]; then
        echo "Packaged runtime verification failed for $binary:" >&2
        echo "$report" >&2
        exit 1
    fi
    grep -q '^engine=FFmpeg 9\.0\.1' <<<"$report"
    grep -q 'libavformat 63\.1\.101' <<<"$report"
    grep -q 'libavcodec 63\.1\.101' <<<"$report"
    grep -q 'libavfilter 12\.1\.101' <<<"$report"
    grep -q 'libavutil 61\.1\.101' <<<"$report"
    grep -qi 'GPL' <<<"$report"
    grep -q '^required_encoders=aac,ac3,libsvtav1,libx264,libx265,mov_text,srt$' <<<"$report"
    grep -q '^stabilization=' <<<"$report"
    grep -q '^muxers=matroska,mp4$' <<<"$report"
}

verify_bundle() {
    local bundle="$1"
    local label="$2"
    local binary="$bundle/Contents/MacOS/VideoFerry"
    local frameworks="$bundle/Contents/Frameworks"
    local resources="$bundle/Contents/Resources"
    local info="$bundle/Contents/Info.plist"

    for required in \
        "$binary" \
        "$info" \
        "$resources/engine-manifest.toml" \
        "$resources/README.md" \
        "$resources/VIDEOFERRY-LICENSE.txt" \
        "$resources/UPGRADING_FFMPEG.md" \
        "$resources/NOTO-SANS-CJK-LICENSE.txt" \
        "$resources/FFMPEG-LICENSE.txt" \
        "$resources/FFMPEG-BUILD-INFO.txt" \
        "$resources/lut/dji/action6.cube" \
        "$resources/lut/dji/pocket3.cube"; do
        if [[ ! -f "$required" ]]; then
            echo "$label is missing required file: $required" >&2
            exit 1
        fi
    done
    if [[ ! -d "$resources/ThirdPartyLicenses" ]]; then
        echo "$label is missing third-party license notices." >&2
        exit 1
    fi
    if find "$bundle" -type f \( -iname 'ffmpeg' -o -iname 'ffprobe' -o -iname 'ffmpeg.exe' -o -iname 'ffprobe.exe' \) -print -quit | grep -q .; then
        echo "$label unexpectedly contains an FFmpeg or ffprobe executable." >&2
        exit 1
    fi

    plutil -lint "$info" >/dev/null
    assert_equal "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$info")" \
        "io.github.infiz.videoferry" "$label bundle identifier mismatch"
    assert_equal "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$info")" \
        "$expected_version" "$label application version mismatch"
    assert_equal "$(/usr/libexec/PlistBuddy -c 'Print :LSMinimumSystemVersion' "$info")" \
        "13.0" "$label deployment target mismatch"

    verify_architecture "$binary"
    while IFS= read -r library; do
        verify_architecture "$library"
    done < <(find "$frameworks" -maxdepth 1 -type f -name '*.dylib' -print)
    verify_dependencies "$bundle"
    codesign --verify --deep --strict "$bundle"
    verify_runtime "$binary"
}

if [[ ! -d "$app_bundle" || ! -f "$archive" || ! -f "$dmg" ]]; then
    echo "Application bundle, ZIP, or DMG is missing." >&2
    exit 1
fi

verify_bundle "$app_bundle" "application bundle"

zip_root="$temporary_root/zip"
mkdir -p "$zip_root"
ditto -x -k "$archive" "$zip_root"
verify_bundle "$zip_root/VideoFerry.app" "portable ZIP"

mkdir -p "$mount_point"
hdiutil attach "$dmg" -nobrowse -readonly -mountpoint "$mount_point" -quiet
mounted=1
verify_bundle "$mount_point/VideoFerry.app" "disk image"
if [[ ! -L "$mount_point/Applications" || "$(readlink "$mount_point/Applications")" != "/Applications" ]]; then
    echo "Disk image is missing the drag-to-install Applications link." >&2
    exit 1
fi
if [[ ! -f "$mount_point/.background/dmg-background.png" || ! -f "$mount_point/.DS_Store" ]]; then
    echo "Disk image is missing its custom Finder installation layout." >&2
    exit 1
fi
hdiutil detach "$mount_point" -quiet
mounted=0

echo "Clean macOS bundle, ZIP, and DMG runtime verification passed."

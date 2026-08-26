#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
workspace_root="$(cd "$script_dir/../.." && pwd)"

if [[ "$(uname -s)" != "Darwin" || "$(uname -m)" != "arm64" ]]; then
    echo "The pinned FFmpeg SDK must be built on Apple Silicon macOS." >&2
    exit 1
fi
if ! command -v brew >/dev/null 2>&1; then
    echo "Homebrew is required to provide the pinned codec build dependencies." >&2
    exit 1
fi

ffmpeg_version="9.0.1"
source_url="https://ffmpeg.org/releases/ffmpeg-$ffmpeg_version.tar.xz"
source_sha256="cf38e0e28c7e5605942c4a77755349b0145804a397af37eb1fb4c77cb237f635"
build_root="${VIDEOFERRY_FFMPEG_BUILD_ROOT:-$workspace_root/.local/ffmpeg/macos-aarch64-build}"
ffmpeg_dir="${FFMPEG_DIR:-$workspace_root/.local/ffmpeg/macos-aarch64}"
allowed_root="$workspace_root/.local/ffmpeg"

resolve_safe_target() {
    local candidate="$1"
    local label="$2"
    case "$candidate" in
        "$allowed_root"/*) ;;
        *) echo "$label must stay under $allowed_root: $candidate" >&2; exit 1 ;;
    esac
    case "/${candidate#"$allowed_root"/}/" in
        */../*|*/./*) echo "$label cannot contain dot path components: $candidate" >&2; exit 1 ;;
    esac
    mkdir -p "$candidate"
    local resolved
    resolved="$(cd "$candidate" && pwd -P)"
    case "$resolved" in
        "$allowed_root"/*) ;;
        *) echo "$label resolves outside $allowed_root: $resolved" >&2; exit 1 ;;
    esac
    printf '%s\n' "$resolved"
}

mkdir -p "$allowed_root"
allowed_root="$(cd "$allowed_root" && pwd -P)"
build_root="$(resolve_safe_target "$build_root" "Build root")"
ffmpeg_dir="$(resolve_safe_target "$ffmpeg_dir" "FFmpeg SDK")"
if [[ "$build_root" == "$ffmpeg_dir" || "$build_root" == "$ffmpeg_dir"/* || "$ffmpeg_dir" == "$build_root"/* ]]; then
    echo "Build root and FFmpeg SDK must be separate sibling directories." >&2
    exit 1
fi

formulae=(pkgconf nasm x264 x265 svt-av1 vid.stab)
for formula in "${formulae[@]}"; do
    if ! brew list --versions "$formula" >/dev/null 2>&1; then
        echo "Missing Homebrew build dependency: $formula" >&2
        echo "Install all dependencies with: brew install ${formulae[*]}" >&2
        exit 1
    fi
done

archive="$build_root/ffmpeg-$ffmpeg_version.tar.xz"
source_dir="$build_root/ffmpeg-$ffmpeg_version"
mkdir -p "$build_root"
if [[ ! -f "$archive" ]]; then
    curl --fail --location --output "$archive" "$source_url"
fi
actual_sha256="$(shasum -a 256 "$archive" | awk '{print $1}')"
if [[ "$actual_sha256" != "$source_sha256" ]]; then
    echo "FFmpeg source checksum mismatch: expected $source_sha256, got $actual_sha256" >&2
    exit 1
fi

rm -rf "$source_dir" "$ffmpeg_dir"
tar -xJf "$archive" -C "$build_root"
mkdir -p "$ffmpeg_dir"

export MACOSX_DEPLOYMENT_TARGET=13.0
export PATH="$(brew --prefix pkgconf)/bin:$(brew --prefix nasm)/bin:$PATH"
pkg_config_paths=()
for formula in x264 x265 svt-av1 vid.stab; do
    prefix="$(brew --prefix "$formula")"
    [[ -d "$prefix/lib/pkgconfig" ]] && pkg_config_paths+=("$prefix/lib/pkgconfig")
done
export PKG_CONFIG_PATH="$(IFS=:; echo "${pkg_config_paths[*]}")"

configure_flags=(
    "--prefix=$ffmpeg_dir"
    "--arch=arm64"
    "--target-os=darwin"
    "--cc=clang"
    "--enable-shared"
    "--disable-static"
    "--disable-programs"
    "--disable-doc"
    "--disable-debug"
    "--enable-gpl"
    "--enable-libx264"
    "--enable-libx265"
    "--enable-libsvtav1"
    "--enable-libvidstab"
    "--enable-videotoolbox"
    "--enable-audiotoolbox"
    "--extra-cflags=-arch arm64 -mmacosx-version-min=13.0"
    "--extra-ldflags=-arch arm64 -mmacosx-version-min=13.0"
)

cd "$source_dir"
./configure "${configure_flags[@]}"
make -j"$(sysctl -n hw.logicalcpu)"
make install

cp COPYING.GPLv3 "$ffmpeg_dir/LICENSE"
mkdir -p "$ffmpeg_dir/licenses"
for formula in x264 x265 svt-av1 vid.stab; do
    prefix="$(brew --prefix "$formula")"
    while IFS= read -r license_file; do
        filename="${formula//./-}-$(basename "$license_file")"
        cp "$license_file" "$ffmpeg_dir/licenses/$filename"
    done < <(find "$prefix" -maxdepth 2 -type f \( -iname 'LICENSE*' -o -iname 'COPYING*' -o -iname 'NOTICE*' \) -print)
done

{
    echo "FFmpeg source: $source_url"
    echo "FFmpeg source SHA-256: $source_sha256"
    echo "Deployment target: $MACOSX_DEPLOYMENT_TARGET"
    echo "Configure flags: ${configure_flags[*]}"
    echo "Homebrew dependencies:"
    brew list --versions "${formulae[@]}"
} > "$ffmpeg_dir/BUILD-INFO.txt"

for required in \
    libavcodec.63.dylib \
    libavfilter.12.dylib \
    libavformat.63.dylib \
    libavutil.61.dylib \
    libswresample.7.dylib \
    libswscale.10.dylib; do
    if [[ ! -e "$ffmpeg_dir/lib/$required" ]]; then
        echo "Pinned SDK build did not produce $required" >&2
        exit 1
    fi
done

echo "Pinned FFmpeg SDK: $ffmpeg_dir"
echo "Next: bash packaging/macos/build.sh"

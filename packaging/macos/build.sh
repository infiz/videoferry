#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
workspace_root="$(cd "$script_dir/../.." && pwd)"
app_version="$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$workspace_root/Cargo.toml" | head -n 1)"
if [[ -z "$app_version" ]]; then
    echo "Unable to read the application version from Cargo.toml." >&2
    exit 1
fi

if [[ "$(uname -s)" != "Darwin" ]]; then
    echo "The macOS package must be built on macOS." >&2
    exit 1
fi
if [[ "$(uname -m)" != "arm64" ]]; then
    echo "This package currently targets Apple Silicon (arm64)." >&2
    exit 1
fi

expected_rust_version="$(awk -F'"' '/^[[:space:]]*channel[[:space:]]*=/{print $2; exit}' "$workspace_root/rust-toolchain.toml")"
actual_rust_version="$(rustc --version)"
if [[ -z "$expected_rust_version" || "$actual_rust_version" != "rustc $expected_rust_version "* ]]; then
    echo "Release builds require rustc $expected_rust_version exactly; active compiler is '$actual_rust_version'." >&2
    exit 1
fi

ffmpeg_dir="${FFMPEG_DIR:-$workspace_root/.local/ffmpeg/macos-aarch64}"
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
for required in include lib; do
    if [[ ! -d "$ffmpeg_dir/$required" ]]; then
        echo "FFmpeg SDK is missing '$required': $ffmpeg_dir" >&2
        exit 1
    fi
done
for required in LICENSE BUILD-INFO.txt licenses; do
    if [[ ! -e "$ffmpeg_dir/$required" ]]; then
        echo "FFmpeg SDK is missing required build/license record: $required" >&2
        exit 1
    fi
done
for required in \
    libavcodec.63.dylib \
    libavfilter.12.dylib \
    libavformat.63.dylib \
    libavutil.61.dylib \
    libswresample.7.dylib \
    libswscale.10.dylib; do
    if [[ ! -e "$ffmpeg_dir/lib/$required" ]]; then
        echo "FFmpeg SDK is missing pinned runtime library: $required" >&2
        exit 1
    fi
done
if [[ -z "$libclang_path" || ! -f "$libclang_path/libclang.dylib" ]]; then
    echo "libclang.dylib was not found; set LIBCLANG_PATH to its directory." >&2
    exit 1
fi

export FFMPEG_DIR="$ffmpeg_dir"
export LIBCLANG_PATH="$libclang_path"

cd "$workspace_root"
cargo build --locked --release -p videoferry-app --features native-ffmpeg

dist_root="$workspace_root/dist/macos"
app_bundle="$dist_root/VideoFerry.app"
frameworks="$app_bundle/Contents/Frameworks"
resources="$app_bundle/Contents/Resources"
macos="$app_bundle/Contents/MacOS"
case "$app_bundle" in
    "$workspace_root"/dist/macos/*) ;;
    *) echo "Refusing to replace an unexpected path: $app_bundle" >&2; exit 1 ;;
esac
rm -rf "$app_bundle"
mkdir -p "$frameworks" "$resources/lut/dji" "$macos"

cp "$workspace_root/target/release/videoferry" "$macos/VideoFerry"
cp "$script_dir/Info.plist" "$app_bundle/Contents/Info.plist"
cp "$workspace_root/engine-manifest.toml" "$resources/"
cp "$workspace_root/README.md" "$resources/"
cp "$workspace_root/LICENSE" "$resources/VIDEOFERRY-LICENSE.txt"
cp "$workspace_root/docs/UPGRADING_FFMPEG.md" "$resources/"
cp "$workspace_root/crates/app/assets/fonts/OFL.txt" "$resources/NOTO-SANS-CJK-LICENSE.txt"
cp "$workspace_root"/assets/lut/dji/*.cube "$resources/lut/dji/"

iconset="$dist_root/AppIcon.iconset"
case "$iconset" in
    "$workspace_root"/dist/macos/*) ;;
    *) echo "Refusing to replace an unexpected iconset path: $iconset" >&2; exit 1 ;;
esac
rm -rf "$iconset"
mkdir -p "$iconset"
icon_source="$workspace_root/crates/app/assets/app-icon.png"
sips -z 16 16 "$icon_source" --out "$iconset/icon_16x16.png" >/dev/null
sips -z 32 32 "$icon_source" --out "$iconset/icon_16x16@2x.png" >/dev/null
sips -z 32 32 "$icon_source" --out "$iconset/icon_32x32.png" >/dev/null
sips -z 64 64 "$icon_source" --out "$iconset/icon_32x32@2x.png" >/dev/null
sips -z 128 128 "$icon_source" --out "$iconset/icon_128x128.png" >/dev/null
sips -z 256 256 "$icon_source" --out "$iconset/icon_128x128@2x.png" >/dev/null
sips -z 256 256 "$icon_source" --out "$iconset/icon_256x256.png" >/dev/null
sips -z 512 512 "$icon_source" --out "$iconset/icon_256x256@2x.png" >/dev/null
sips -z 512 512 "$icon_source" --out "$iconset/icon_512x512.png" >/dev/null
cp "$icon_source" "$iconset/icon_512x512@2x.png"
iconutil -c icns "$iconset" -o "$resources/AppIcon.icns"
rm -rf "$iconset"

cp "$ffmpeg_dir/LICENSE" "$resources/FFMPEG-LICENSE.txt"
cp "$ffmpeg_dir/BUILD-INFO.txt" "$resources/FFMPEG-BUILD-INFO.txt"
cp -R "$ffmpeg_dir/licenses" "$resources/ThirdPartyLicenses"

while IFS= read -r library; do
    cp -P "$library" "$frameworks/"
done < <(find "$ffmpeg_dir/lib" -maxdepth 1 \( -type f -o -type l \) -name '*.dylib' -print)

# Collect non-system codec dylibs (for example Homebrew x264/x265/SVT-AV1)
# transitively. The queue file makes the loop compatible with macOS Bash 3.2.
dependency_queue="$dist_root/dylib-dependency-queue.txt"
: > "$dependency_queue"
find "$frameworks" -maxdepth 1 -type f -name '*.dylib' -print >> "$dependency_queue"
while IFS= read -r binary; do
    while IFS= read -r dependency; do
        case "$dependency" in
            /System/*|/usr/lib/*|@executable_path/*|@loader_path/*) continue ;;
        esac
        basename="$(basename "$dependency")"
        [[ -e "$frameworks/$basename" ]] && continue
        candidate=""
        if [[ "$dependency" == /* && -f "$dependency" ]]; then
            candidate="$dependency"
        elif [[ "$dependency" == @rpath/* && -f "$ffmpeg_dir/lib/$basename" ]]; then
            candidate="$ffmpeg_dir/lib/$basename"
        fi
        if [[ -z "$candidate" ]]; then
            echo "Unable to resolve non-system dylib dependency $dependency from $binary" >&2
            rm -f "$dependency_queue"
            exit 1
        fi
        cp -L "$candidate" "$frameworks/$basename"
        chmod u+w "$frameworks/$basename"
        echo "$frameworks/$basename" >> "$dependency_queue"
    done < <(otool -L "$binary" | tail -n +2 | awk '{print $1}')
done < "$dependency_queue"
rm -f "$dependency_queue"

rewrite_dependencies() {
    local binary="$1"
    while IFS= read -r dependency; do
        local basename
        basename="$(basename "$dependency")"
        if [[ -e "$frameworks/$basename" && "$dependency" != "@rpath/$basename" ]]; then
            install_name_tool -change "$dependency" "@rpath/$basename" "$binary"
        fi
    done < <(otool -L "$binary" | tail -n +2 | awk '{print $1}')
}

while IFS= read -r library; do
    install_name_tool -id "@rpath/$(basename "$library")" "$library"
    rewrite_dependencies "$library"
done < <(find "$frameworks" -maxdepth 1 -type f -name '*.dylib' -print)

app_binary="$macos/VideoFerry"
rewrite_dependencies "$app_binary"
if ! otool -l "$app_binary" | grep -q '@executable_path/../Frameworks'; then
    install_name_tool -add_rpath '@executable_path/../Frameworks' "$app_binary"
fi

while IFS= read -r binary; do
    while IFS= read -r dependency; do
        case "$dependency" in
            /System/*|/usr/lib/*|@rpath/*) ;;
            *) echo "Unbundled dependency remains in $binary: $dependency" >&2; exit 1 ;;
        esac
        if [[ "$dependency" == @rpath/* && ! -e "$frameworks/$(basename "$dependency")" ]]; then
            echo "Bundled dependency is missing for $binary: $dependency" >&2
            exit 1
        fi
    done < <(otool -L "$binary" | tail -n +2 | awk '{print $1}')
done < <(printf '%s\n' "$app_binary"; find "$frameworks" -maxdepth 1 -type f -name '*.dylib' -print)

signing_identity="${VIDEOFERRY_CODESIGN_IDENTITY:--}"
if [[ "$signing_identity" == "-" ]]; then
    signing_arguments=(--force --sign - --timestamp=none)
else
    signing_arguments=(--force --sign "$signing_identity" --options runtime --timestamp)
fi
while IFS= read -r library; do
    codesign "${signing_arguments[@]}" "$library"
done < <(find "$frameworks" -maxdepth 1 -type f -name '*.dylib' -print)
codesign "${signing_arguments[@]}" --deep "$app_bundle"
codesign --verify --deep --strict "$app_bundle"

archive="$dist_root/VideoFerry-$app_version-macos-aarch64.zip"
rm -f "$archive"
ditto -c -k --sequesterRsrc --keepParent "$app_bundle" "$archive"

dmg_root="$dist_root/dmg-root"
dmg_read_write="$dist_root/VideoFerry-$app_version-macos-aarch64-read-write.dmg"
dmg="$dist_root/VideoFerry-$app_version-macos-aarch64.dmg"
dmg_background="$script_dir/dmg-background.png"
dmg_volume_name="VideoFerry Installer"
for temporary_path in "$dmg_root" "$dmg_read_write"; do
    case "$temporary_path" in
        "$workspace_root"/dist/macos/*) ;;
        *) echo "Refusing to replace an unexpected path: $temporary_path" >&2; exit 1 ;;
    esac
done
if [[ ! -f "$dmg_background" ]]; then
    echo "The DMG background is missing: $dmg_background" >&2
    exit 1
fi

dmg_device=""
dmg_mount=""
dmg_mounted=0
cleanup_dmg_build() {
    if [[ "$dmg_mounted" == "1" ]]; then
        hdiutil detach "$dmg_device" -quiet || true
    fi
    rm -rf "$dmg_root"
    rm -f "$dmg_read_write"
}
trap cleanup_dmg_build EXIT

rm -rf "$dmg_root"
rm -f "$dmg" "$dmg_read_write"
mkdir -p "$dmg_root/.background"
cp -R "$app_bundle" "$dmg_root/"
cp "$dmg_background" "$dmg_root/.background/dmg-background.png"
ln -s /Applications "$dmg_root/Applications"

# Build a writable image first so Finder can persist the custom icon layout.
hdiutil create \
    -volname "$dmg_volume_name" \
    -srcfolder "$dmg_root" \
    -ov \
    -format UDRW \
    "$dmg_read_write" >/dev/null
attach_output="$(hdiutil attach \
    "$dmg_read_write" \
    -readwrite \
    -noverify \
    -noautoopen)"
dmg_device="$(awk '/^\/dev\// { print $1; exit }' <<<"$attach_output")"
dmg_mount="$(awk -F $'\t' '$NF ~ /^\// { mount = $NF } END { print mount }' <<<"$attach_output")"
if [[ -z "$dmg_device" || -z "$dmg_mount" || ! -d "$dmg_mount" ]]; then
    echo "Unable to determine the device or mount point for the writable DMG." >&2
    exit 1
fi
dmg_mounted=1

mounted_volume_name="$(basename "$dmg_mount")"
osascript - "$mounted_volume_name" <<'APPLESCRIPT'
on run argv
    set volumeName to item 1 of argv
    tell application "Finder"
        set dmgDisk to disk volumeName
        open dmgDisk
        set dmgWindow to container window of dmgDisk
        set current view of dmgWindow to icon view
        set toolbar visible of dmgWindow to false
        set statusbar visible of dmgWindow to false
        set bounds of dmgWindow to {100, 100, 760, 500}

        set iconOptions to icon view options of dmgWindow
        set arrangement of iconOptions to not arranged
        set icon size of iconOptions to 112
        set text size of iconOptions to 13
        set background picture of iconOptions to file ".background:dmg-background.png" of dmgDisk

        set position of item "VideoFerry.app" of dmgDisk to {150, 190}
        set position of item "Applications" of dmgDisk to {510, 190}
        update dmgDisk without registering applications
        delay 2
        close dmgWindow
        open dmgDisk
        delay 2
    end tell
end run
APPLESCRIPT

sync
hdiutil detach "$dmg_device" -quiet
dmg_mounted=0
hdiutil convert \
    "$dmg_read_write" \
    -format UDZO \
    -imagekey zlib-level=9 \
    -o "$dmg" >/dev/null
if [[ ! -f "$dmg" ]]; then
    echo "DMG conversion did not produce the expected installation image: $dmg" >&2
    exit 1
fi
cleanup_dmg_build
trap - EXIT

if [[ "$signing_identity" != "-" ]]; then
    codesign "${signing_arguments[@]}" "$dmg"
fi
if [[ -n "${VIDEOFERRY_NOTARY_PROFILE:-}" ]]; then
    xcrun notarytool submit "$dmg" --keychain-profile "$VIDEOFERRY_NOTARY_PROFILE" --wait
    xcrun stapler staple "$dmg"
fi

bash "$script_dir/verify-package.sh" "$app_bundle" "$archive" "$dmg"

echo "Application bundle: $app_bundle"
echo "Archive: $archive"
echo "Disk image: $dmg"

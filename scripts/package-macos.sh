#!/bin/sh
set -eu

project_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
bundle="$project_dir/dist/Pocket Music.app"
contents="$bundle/Contents"
version=${POCKET_YTM_VERSION:-$(cargo metadata --manifest-path "$project_dir/Cargo.toml" --no-deps --format-version 1 | python3 -c 'import json,sys; print(json.load(sys.stdin)["packages"][0]["version"])')}
binary_dir=${POCKET_YTM_BINARY_DIR:-"$project_dir/target/release"}
dependency_dir=${POCKET_YTM_DEPENDENCY_DIR:-"$project_dir/target/macos-dependencies"}

if [ -z "${POCKET_YTM_BINARY_DIR:-}" ]; then
    cargo build --manifest-path "$project_dir/Cargo.toml" --release --bins
fi
if [ ! -x "$dependency_dir/libexec/pocket-ytm-bridge/pocket-ytm-bridge" ] || \
   [ ! -x "$dependency_dir/libexec/pocket-ytm-resolver/pocket-ytm-resolver" ] || \
   [ ! -x "$dependency_dir/bin/ffmpeg" ] || \
   [ ! -x "$dependency_dir/bin/ffprobe" ] || \
   [ ! -x "$dependency_dir/bin/deno" ] || \
   [ ! -f "$dependency_dir/share/bgutil-ytdlp-pot-provider/server/src/generate_once.ts" ]; then
    POCKET_YTM_DEPENDENCY_DIR="$dependency_dir" "$project_dir/scripts/build-macos-dependencies.sh"
fi

rm -rf "$bundle"
install -d \
    "$contents/MacOS" \
    "$contents/Resources/bin" \
    "$contents/Resources/libexec" \
    "$contents/Resources/licenses" \
    "$contents/Resources/share"
install -m 755 "$binary_dir/pocket-ytm" "$contents/MacOS/pocket-ytm"
install -m 755 "$binary_dir/pocket-ytm-updater" "$contents/MacOS/pocket-ytm-updater"
install -m 644 "$project_dir/packaging/macos/Info.plist" "$contents/Info.plist"
for dependency in ffmpeg ffprobe deno; do
    install -m 755 "$dependency_dir/bin/$dependency" "$contents/Resources/bin/$dependency"
done
for dependency in pocket-ytm-bridge pocket-ytm-resolver; do
    # The v0.1.0-v0.1.2 updater extracts ZIP entries as regular files and does
    # not recreate symlinks. Dereference PyInstaller's framework links so those
    # clients can install the bundle without invalidating its code signature.
    /bin/cp -RL \
        "$dependency_dir/libexec/$dependency" \
        "$contents/Resources/libexec/$dependency"
done
/bin/cp -RL \
    "$dependency_dir/share/bgutil-ytdlp-pot-provider" \
    "$contents/Resources/share/bgutil-ytdlp-pot-provider"
find "$dependency_dir/licenses" -maxdepth 1 -type f -exec install -m 644 {} "$contents/Resources/licenses" \;
install -m 644 "$dependency_dir/versions.txt" "$contents/Resources/dependency-versions.txt"

/usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $version" "$contents/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleVersion $version" "$contents/Info.plist"

if command -v codesign >/dev/null 2>&1; then
    codesign --force --deep --sign - "$bundle"
fi

install -m 755 "$binary_dir/pocket-ytm-e2e" "$project_dir/dist/pocket-ytm-e2e"

echo "Created Pocket Music v$version at $bundle"

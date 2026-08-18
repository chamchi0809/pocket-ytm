#!/bin/sh
set -eu

project_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
bundle="$project_dir/dist/Pocket Music.app"
contents="$bundle/Contents"
version=${POCKET_YTM_VERSION:-$(cargo metadata --manifest-path "$project_dir/Cargo.toml" --no-deps --format-version 1 | python3 -c 'import json,sys; print(json.load(sys.stdin)["packages"][0]["version"])')}
binary_dir=${POCKET_YTM_BINARY_DIR:-"$project_dir/target/release"}

if [ -z "${POCKET_YTM_BINARY_DIR:-}" ]; then
    cargo build --manifest-path "$project_dir/Cargo.toml" --release --bins
fi

rm -rf "$bundle"
install -d "$contents/MacOS" "$contents/Resources/backend"
install -m 755 "$binary_dir/pocket-ytm" "$contents/MacOS/pocket-ytm"
install -m 755 "$binary_dir/pocket-ytm-updater" "$contents/MacOS/pocket-ytm-updater"
install -m 644 "$project_dir/packaging/macos/Info.plist" "$contents/Info.plist"
install -m 755 "$project_dir/backend/ytmusic_bridge.py" "$contents/Resources/backend/ytmusic_bridge.py"
install -m 644 "$project_dir/requirements.txt" "$contents/Resources/requirements.txt"

/usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $version" "$contents/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleVersion $version" "$contents/Info.plist"

if command -v codesign >/dev/null 2>&1; then
    codesign --force --deep --sign - "$bundle"
fi

echo "Created Pocket Music v$version at $bundle"

#!/bin/sh
set -eu

project_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
output_dir=${POCKET_YTM_DEPENDENCY_DIR:-"$project_dir/target/macos-dependencies"}
python_bin=${POCKET_YTM_BUILD_PYTHON:-"$project_dir/.venv/bin/python"}
jobs=${POCKET_YTM_BUILD_JOBS:-3}

yt_dlp_version=2026.07.04
pot_provider_version=1.3.1
pot_provider_sha256=b5400343482820062372e997cc0c9ace18637f8e774cf70b22c58b89b99e9abe
deno_version=2.9.5
deno_sha256=b796aadd131f6930560c1ee040cf0d6f53933fbb987464e9ff46bd7ea4830615
ffmpeg_version=9.0.1
ffmpeg_sha256=cf38e0e28c7e5605942c4a77755349b0145804a397af37eb1fb4c77cb237f635

if [ ! -x "$python_bin" ]; then
    echo "Build Python not found: $python_bin" >&2
    echo "Run ./scripts/bootstrap.sh or set POCKET_YTM_BUILD_PYTHON." >&2
    exit 1
fi
if [ "$(uname -m)" != "arm64" ]; then
    echo "Bundled macOS dependencies must be built on Apple Silicon." >&2
    exit 1
fi

work_dir=$(mktemp -d "${TMPDIR:-/tmp}/pocket-music-dependencies.XXXXXX")
cleanup() {
    rm -rf "$work_dir"
}
trap cleanup EXIT HUP INT TERM

install -d "$output_dir/bin" "$output_dir/libexec" "$output_dir/licenses" "$output_dir/share"

"$python_bin" -m pip install --disable-pip-version-check -r "$project_dir/requirements-build-macos.txt"

download() {
    url=$1
    destination=$2
    curl --fail --location --retry 3 --silent --show-error "$url" --output "$destination"
}

verify_sha256() {
    expected=$1
    file=$2
    actual=$(shasum -a 256 "$file" | awk '{print $1}')
    if [ "$actual" != "$expected" ]; then
        echo "SHA-256 mismatch for $file" >&2
        exit 1
    fi
}

"$python_bin" -m PyInstaller \
    --noconfirm \
    --clean \
    --onedir \
    --target-architecture arm64 \
    --collect-all ytmusicapi \
    --name pocket-ytm-bridge \
    --distpath "$work_dir/bridge-dist" \
    --workpath "$work_dir/bridge-work" \
    --specpath "$work_dir/bridge-spec" \
    "$project_dir/backend/ytmusic_bridge.py"
rm -rf "$output_dir/libexec/pocket-ytm-bridge"
ditto \
    "$work_dir/bridge-dist/pocket-ytm-bridge" \
    "$output_dir/libexec/pocket-ytm-bridge"
chmod 755 "$output_dir/libexec/pocket-ytm-bridge/pocket-ytm-bridge"

"$python_bin" -m PyInstaller \
    --noconfirm \
    --clean \
    --onedir \
    --target-architecture arm64 \
    --collect-all yt_dlp \
    --collect-all yt_dlp_ejs \
    --copy-metadata yt-dlp \
    --name pocket-ytm-resolver \
    --distpath "$work_dir/resolver-dist" \
    --workpath "$work_dir/resolver-work" \
    --specpath "$work_dir/resolver-spec" \
    "$project_dir/backend/yt_dlp_resolver.py"
rm -rf "$output_dir/libexec/pocket-ytm-resolver"
ditto \
    "$work_dir/resolver-dist/pocket-ytm-resolver" \
    "$output_dir/libexec/pocket-ytm-resolver"
chmod 755 "$output_dir/libexec/pocket-ytm-resolver/pocket-ytm-resolver"

pot_provider_archive="$work_dir/bgutil-ytdlp-pot-provider.tar.gz"
download \
    "https://github.com/Brainicism/bgutil-ytdlp-pot-provider/archive/refs/tags/$pot_provider_version.tar.gz" \
    "$pot_provider_archive"
verify_sha256 "$pot_provider_sha256" "$pot_provider_archive"
tar -xzf "$pot_provider_archive" -C "$work_dir"
pot_provider_source="$work_dir/bgutil-ytdlp-pot-provider-$pot_provider_version"
patch -d "$pot_provider_source" -p1 \
    < "$project_dir/packaging/patches/bgutil-deno-offline.patch"
(
    cd "$pot_provider_source/server"
    npm ci --omit=dev --no-audit --no-fund
)
pot_plugin_archive="$work_dir/bgutil-ytdlp-pot-provider.zip"
(
    cd "$pot_provider_source/plugin"
    zip -q -r "$pot_plugin_archive" yt_dlp_plugins
)
install -d "$output_dir/libexec/pocket-ytm-resolver/yt-dlp-plugins"
install -m 644 \
    "$pot_plugin_archive" \
    "$output_dir/libexec/pocket-ytm-resolver/yt-dlp-plugins/bgutil-ytdlp-pot-provider.zip"
rm -rf "$pot_provider_source/server/node_modules/.bin"
rm -rf "$output_dir/share/bgutil-ytdlp-pot-provider"
ditto "$pot_provider_source" "$output_dir/share/bgutil-ytdlp-pot-provider"

deno_archive="$work_dir/deno.zip"
download \
    "https://github.com/denoland/deno/releases/download/v$deno_version/deno-aarch64-apple-darwin.zip" \
    "$deno_archive"
verify_sha256 "$deno_sha256" "$deno_archive"
ditto -x -k "$deno_archive" "$work_dir/deno"
install -m 755 "$work_dir/deno/deno" "$output_dir/bin/deno"

ffmpeg_archive="$work_dir/ffmpeg.tar.xz"
download "https://ffmpeg.org/releases/ffmpeg-$ffmpeg_version.tar.xz" "$ffmpeg_archive"
verify_sha256 "$ffmpeg_sha256" "$ffmpeg_archive"
tar -xf "$ffmpeg_archive" -C "$work_dir"
(
    cd "$work_dir/ffmpeg-$ffmpeg_version"
    MACOSX_DEPLOYMENT_TARGET=12.0 ./configure \
        --arch=arm64 \
        --target-os=darwin \
        --cc=clang \
        --disable-autodetect \
        --disable-doc \
        --disable-debug \
        --disable-shared \
        --enable-static \
        --disable-programs \
        --enable-ffmpeg \
        --enable-ffprobe \
        --enable-securetransport \
        --extra-cflags=-mmacosx-version-min=12.0 \
        --extra-ldflags=-mmacosx-version-min=12.0
    make -j"$jobs" ffmpeg ffprobe
)
install -m 755 "$work_dir/ffmpeg-$ffmpeg_version/ffmpeg" "$output_dir/bin/ffmpeg"
install -m 755 "$work_dir/ffmpeg-$ffmpeg_version/ffprobe" "$output_dir/bin/ffprobe"

install -m 644 \
    "$project_dir/packaging/THIRD_PARTY_NOTICES.md" \
    "$output_dir/licenses/THIRD_PARTY_NOTICES.md"
install -m 644 \
    "$work_dir/ffmpeg-$ffmpeg_version/COPYING.LGPLv2.1" \
    "$output_dir/licenses/FFmpeg-LGPL-2.1.txt"
download \
    "https://raw.githubusercontent.com/yt-dlp/yt-dlp/$yt_dlp_version/THIRD_PARTY_LICENSES.txt" \
    "$output_dir/licenses/yt-dlp-THIRD_PARTY_LICENSES.txt"
download \
    "https://raw.githubusercontent.com/denoland/deno/v$deno_version/LICENSE.md" \
    "$output_dir/licenses/Deno-LICENSE.md"
download \
    "https://raw.githubusercontent.com/sigma67/ytmusicapi/1.12.2/LICENSE" \
    "$output_dir/licenses/ytmusicapi-LICENSE.txt"
download \
    "https://raw.githubusercontent.com/websocket-client/websocket-client/v1.9.0/LICENSE" \
    "$output_dir/licenses/websocket-client-Apache-2.0.txt"
install -m 644 \
    "$pot_provider_source/LICENSE" \
    "$output_dir/licenses/bgutil-ytdlp-pot-provider-GPL-3.0.txt"

canvas_module="$output_dir/share/bgutil-ytdlp-pot-provider/server/node_modules/canvas/build/Release/canvas.node"
test -f "$canvas_module"
lipo "$canvas_module" -verify_arch arm64
if otool -L "$canvas_module" | tail -n +2 | grep -E '/opt/homebrew|/usr/local|/Users/' >/dev/null; then
    echo "$canvas_module links a non-bundled dependency" >&2
    otool -L "$canvas_module" >&2
    exit 1
fi

for binary in \
    "$output_dir/libexec/pocket-ytm-bridge/pocket-ytm-bridge" \
    "$output_dir/libexec/pocket-ytm-resolver/pocket-ytm-resolver" \
    "$output_dir/bin/deno" \
    "$output_dir/bin/ffmpeg" \
    "$output_dir/bin/ffprobe"
do
    lipo "$binary" -verify_arch arm64
    if otool -L "$binary" | tail -n +2 | grep -E '/opt/homebrew|/usr/local|/Users/' >/dev/null; then
        echo "$binary links a non-system dependency" >&2
        otool -L "$binary" >&2
        exit 1
    fi
done

{
    echo "ytmusicapi=1.12.2"
    echo "yt-dlp=$yt_dlp_version"
    echo "bgutil-ytdlp-pot-provider=$pot_provider_version"
    echo "websocket-client=1.9.0"
    echo "deno=$deno_version"
    echo "ffmpeg=$ffmpeg_version"
    "$python_bin" --version
} > "$output_dir/versions.txt"

echo "Created self-contained Apple Silicon dependencies at $output_dir"

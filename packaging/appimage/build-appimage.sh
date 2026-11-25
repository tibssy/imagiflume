#!/usr/bin/env bash
# Build a self-contained AppImage for Imagiflume.
#
# Bundles GTK 4, libadwaita and their runtime data with linuxdeploy. The
# resulting AppImage still uses the host glibc (2.41+ for the Debian 13
# release build), GPU drivers and fonts.
#
# Usage:
#   packaging/appimage/build-appimage.sh \
#     --binary target/release/imagiflume \
#     --outdir dist
#
# linuxdeploy and the GTK plugin are downloaded into a cache directory and
# verified against pinned SHA-256 sums. Set KEEP_APPDIR=1 to inspect the
# intermediate AppDir, or APPIMAGE_TOOLS to reuse a tools directory.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BINARY=""
OUTDIR=""
TOOLS="${APPIMAGE_TOOLS:-${XDG_CACHE_HOME:-$HOME/.cache}/imagiflume-appimage-tools}"

LINUXDEPLOY_URL="https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/linuxdeploy-x86_64.AppImage"
# `continuous` is a rolling release. Pinning the current asset's SHA makes a
# silent change fail loudly instead of producing a different bundle.
LINUXDEPLOY_SHA="df908b19f1ed1cdafa79508d415aa12d6c56ce4d56d4149aa8c09286718706e5"
GTK_PLUGIN_COMMIT="7a3fbc31a9e5075073ff8790f26effbac5f84453"
GTK_PLUGIN_URL="https://raw.githubusercontent.com/linuxdeploy/linuxdeploy-plugin-gtk/${GTK_PLUGIN_COMMIT}/linuxdeploy-plugin-gtk.sh"
GTK_PLUGIN_SHA="b0f4cbc684a0103a9651f0955b635eaea0096b3a66c0f5a2c2aa337960375171"

usage() {
    sed -n '2,16p' "$0" | sed 's/^# \{0,1\}//'
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --binary)
            BINARY="${2:?missing value for --binary}"
            shift 2
            ;;
        --outdir)
            OUTDIR="${2:?missing value for --outdir}"
            shift 2
            ;;
        --tools)
            TOOLS="${2:?missing value for --tools}"
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "unknown argument: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

if [[ -z "$BINARY" || -z "$OUTDIR" ]]; then
    usage >&2
    exit 2
fi
if [[ ! -x "$BINARY" ]]; then
    echo "not an executable: $BINARY" >&2
    exit 1
fi
for tool in curl rsvg-convert sha256sum file pkg-config; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "missing required tool: $tool" >&2
        exit 1
    fi
done

mkdir -p "$OUTDIR" "$TOOLS"
OUTDIR="$(cd "$OUTDIR" && pwd)"

download() {
    local url="$1" path="$2" expected="$3"
    if [[ -f "$path" ]] && echo "$expected  $path" | sha256sum -c --status; then
        return
    fi
    echo "downloading $(basename "$path")…"
    curl --fail --location --retry 3 --silent --show-error "$url" -o "$path.tmp"
    echo "$expected  $path.tmp" | sha256sum -c --status
    mv "$path.tmp" "$path"
}

download "$LINUXDEPLOY_URL" "$TOOLS/linuxdeploy" "$LINUXDEPLOY_SHA"
download "$GTK_PLUGIN_URL" "$TOOLS/linuxdeploy-plugin-gtk.sh" "$GTK_PLUGIN_SHA"
chmod +x "$TOOLS/linuxdeploy" "$TOOLS/linuxdeploy-plugin-gtk.sh"

WORK="$(mktemp -d "${TMPDIR:-/tmp}/imagiflume-appimage.XXXXXX")"
cleanup() {
    if [[ "${KEEP_APPDIR:-0}" != "1" ]]; then
        rm -rf "$WORK"
    else
        echo "AppDir kept at $WORK/AppDir" >&2
    fi
}
trap cleanup EXIT

APPDIR="$WORK/AppDir"
mkdir -p \
    "$APPDIR/usr/bin" \
    "$APPDIR/usr/share/applications" \
    "$APPDIR/usr/share/icons/hicolor/scalable/apps" \
    "$APPDIR/usr/share/icons/hicolor/256x256/apps" \
    "$APPDIR/usr/share/metainfo" \
    "$APPDIR/apprun-hooks"

cp "$BINARY" "$APPDIR/usr/bin/imagiflume"
cp "$ROOT/packaging/io.github.tibssy.Imagiflume.desktop" \
    "$APPDIR/usr/share/applications/"
cp "$ROOT/packaging/io.github.tibssy.Imagiflume.svg" \
    "$APPDIR/usr/share/icons/hicolor/scalable/apps/"
cp "$ROOT/packaging/io.github.tibssy.Imagiflume.metainfo.xml" \
    "$APPDIR/usr/share/metainfo/"
rsvg-convert -w 256 -h 256 \
    "$ROOT/packaging/io.github.tibssy.Imagiflume.svg" \
    -o "$APPDIR/usr/share/icons/hicolor/256x256/apps/io.github.tibssy.Imagiflume.png"

# linuxdeploy-plugin-gtk forces GDK_BACKEND=x11. Imagiflume supports Wayland,
# and forcing XWayland breaks accelerated rendering on some NVIDIA setups.
cat > "$APPDIR/apprun-hooks/zz-imagiflume-wayland.sh" <<'EOF'
unset GDK_BACKEND
EOF

cd "$WORK"
PATH="$TOOLS:$PATH" \
APPIMAGE_EXTRACT_AND_RUN=1 \
DEPLOY_GTK_VERSION=4 \
"$TOOLS/linuxdeploy" \
    --appdir "$APPDIR" \
    --executable "$APPDIR/usr/bin/imagiflume" \
    --desktop-file "$APPDIR/usr/share/applications/io.github.tibssy.Imagiflume.desktop" \
    --icon-file "$APPDIR/usr/share/icons/hicolor/256x256/apps/io.github.tibssy.Imagiflume.png" \
    --plugin gtk \
    --output appimage

BUILT="$(find "$WORK" -maxdepth 1 -name '*.AppImage' -type f -print -quit)"
if [[ -z "$BUILT" ]]; then
    echo "linuxdeploy did not produce an AppImage" >&2
    exit 1
fi

APPIMAGE="$OUTDIR/imagiflume-linux-x86_64.AppImage"
mv "$BUILT" "$APPIMAGE"
chmod +x "$APPIMAGE"
(
    cd "$OUTDIR"
    sha256sum "$(basename "$APPIMAGE")" > "$(basename "$APPIMAGE").sha256"
)
echo "built $APPIMAGE"

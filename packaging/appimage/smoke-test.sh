#!/usr/bin/env bash
# Verify an Imagiflume AppImage without installing it.
#
# - extracts the image and checks that every ELF dependency resolves
# - checks that GTK4, libadwaita and the Wayland override are bundled
# - starts the app under Xvfb for 12 seconds (124 means it stayed alive)
set -euo pipefail

IMAGE="${1:?usage: smoke-test.sh /path/to/imagiflume.AppImage}"
if [[ ! -x "$IMAGE" ]]; then
    echo "not executable: $IMAGE" >&2
    exit 1
fi
for tool in xvfb-run dbus-run-session timeout ldd; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "missing required tool: $tool" >&2
        exit 1
    fi
done

IMAGE="$(cd "$(dirname "$IMAGE")" && pwd)/$(basename "$IMAGE")"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/imagiflume-appimage-smoke.XXXXXX")"
cleanup() {
    pkill -x AppRun.wrapped 2>/dev/null || true
    pkill -x imagiflume 2>/dev/null || true
    rm -rf "$WORK"
}
trap cleanup EXIT

cd "$WORK"
"$IMAGE" --appimage-extract >/dev/null
if ldd squashfs-root/usr/bin/imagiflume | grep "not found"; then
    echo "the AppImage has unresolved shared libraries" >&2
    exit 1
fi
test -f squashfs-root/usr/lib/libgtk-4.so.1
test -f squashfs-root/usr/lib/libadwaita-1.so.0
grep -R "unset GDK_BACKEND" squashfs-root/apprun-hooks

set +e
timeout --kill-after=2 12 dbus-run-session -- xvfb-run -a \
    env APPIMAGE_EXTRACT_AND_RUN=1 GSK_RENDERER=cairo LIBGL_ALWAYS_SOFTWARE=1 \
    "$IMAGE" > "$WORK/run.log" 2>&1
rc=$?
set -e
cat "$WORK/run.log"

if [[ "$rc" -ne 124 ]]; then
    echo "the AppImage exited with $rc instead of staying alive" >&2
    exit 1
fi
echo "AppImage smoke test passed"

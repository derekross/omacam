#!/usr/bin/env bash
# Screenshots of the UI on synthetic frames, at the size the app gets on
# the Pixel 7 Pro (456x900 logical: the 480x1040 screen minus the shell's
# bars), in a headless Weston's kiosk shell inside a Docker container:
# never on your own desktop. SIZE=WxH changes it. (The phone draws at
# scale 3; headless Weston's --scale doesn't reach GTK, so the shots are
# at scale 1 with the same layout.)
# Writes target/ui-shots/*.png. Needs Docker; builds the app (no libcamera).
set -euo pipefail
cd "$(dirname "$0")/.."
IMAGE=omacam-uitest:2
if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
  docker build -t "$IMAGE" - <<'DOCKERFILE'
FROM archlinux:latest
RUN pacman -Syu --noconfirm --needed weston gtk4 libadwaita adwaita-icon-theme libjpeg-turbo ttf-dejavu mesa gstreamer gst-plugins-base gst-plugins-good gst-plugins-ugly && pacman -Scc --noconfirm
DOCKERFILE
fi
cargo build --release --locked -p omacam
out=target/ui-shots
rm -rf "$out"; mkdir -p "$out"; chmod 777 "$out"
size=${SIZE:-456x900}; scale=1
w=$(( ${size%x*} * scale )); h=$(( ${size#*x} * scale ))
docker run --rm --user "$(id -u):$(id -g)" -e W="$w" -e H="$h" -e SCALE="$scale" \
  -v "$PWD/target/release/omacam:/omacam:ro" -v "$PWD/$out:/out" "$IMAGE" bash -c '
  export XDG_RUNTIME_DIR=/tmp/rt; mkdir -m 700 -p $XDG_RUNTIME_DIR
  weston --backend=headless --shell=kiosk --width=$W --height=$H --scale=$SCALE \
    --socket=wayland-9 --idle-time=0 >/tmp/weston.log 2>&1 &
  sleep 2
  WAYLAND_DISPLAY=wayland-9 GSK_RENDERER=cairo timeout 60 /omacam --fake --debug --output-dir /out/photos --script "
    wait 2; shot /out/main-1x.png; zoom 2; wait 1; shot /out/main-2x.png;
    pinch 0.6; wait 1.5; shot /out/ultrawide.png; zoom 5; wait 1.5; shot /out/tele.png;
    shutter; wait 3; shot /out/after-photo.png; zoom 1; wait 1.5; tap 0.3 0.3; wait 0.3;
    shot /out/focus.png; settings photo; wait 1; shot /out/settings-photo.png;
    settings video; wait 0.5; shot /out/settings-video.png; settings advanced; wait 0.5;
    shot /out/settings-advanced.png; close-settings; wait 0.5; flip; wait 1.5;
    shot /out/front.png; flip; wait 1.5;
    tap 0.3 0.35; wait 0.3; ev 1.2; wait 0.3; shot /out/tap-ev.png; lock; wait 0.5; shot /out/locked.png;
    tap 0.5 0.5; pro on; wait 0.5; shot /out/pro.png; pro-edit shutter; wait 0.5; pro-shutter 4000; wait 0.8;
    shot /out/pro-shutter.png; pro-edit wb; wait 0.5; shot /out/pro-wb.png; pro off; wait 0.3;
    mode video; wait 0.5; shot /out/video-mode.png;
    shutter; wait 2; zoom 2; wait 1; shot /out/recording.png; shutter; wait 3;
    shot /out/after-video.png; quit"
  # A dim room whose cameras offer 60 fps: the low-light fallback at 5x,
  # the controls turned for a phone held sideways, and a recording that
  # switches lenses (main, ultrawide, main) as it goes.
  WAYLAND_DISPLAY=wayland-9 GSK_RENDERER=cairo timeout 60 /omacam --fake --fake-light 0.05 --fake-fps 60 \
    --output-dir /out/dim --script "
    wait 2; zoom 5; wait 4; shot /out/low-light.png; hold left; wait 0.6; shot /out/held-left.png;
    pro on; wait 0.6; shot /out/held-left-pro.png; pro off; hold normal; wait 0.5; zoom 1; mode video; wait 1.5;
    shot /out/video-60.png; shutter; wait 2; zoom 0.5; wait 2; shot /out/recording-switched.png;
    zoom 1; wait 2; shutter; wait 3; quit"
  for v in /out/photos/VID_*.mp4 /out/dim/VID_*.mp4; do
    gst-discoverer-1.0 "$v" 2>/dev/null | grep -E "Duration|video #|Location|geo" | sed "s|^|$(basename $v): |"
  done
' 2>&1 | grep -E "WARN|ERROR|window saved|VID_|recording from another camera" || true
ls "$out"
videos=$(ls "$out"/photos/VID_*.mp4 "$out"/dim/VID_*.mp4 2>/dev/null | wc -l)
if [[ $videos -ge 2 ]]; then
  ls -l "$out"/photos/VID_*.mp4 "$out"/dim/VID_*.mp4
else
  echo "expected two videos, got $videos" >&2
  exit 1
fi

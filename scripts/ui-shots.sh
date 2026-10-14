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
    shot /out/front.png; flip; wait 1.5; mode video; wait 0.5; shot /out/video-mode.png;
    shutter; wait 2; zoom 2; wait 1; shot /out/recording.png; shutter; wait 3;
    shot /out/after-video.png; quit"
' 2>&1 | grep -E "WARN|ERROR|window saved" || true
ls "$out"
video=$(ls "$out"/photos/VID_*.mp4 2>/dev/null | head -1 || true)
if [[ -n $video ]]; then
  echo "recorded: $video ($(stat -c %s "$video") bytes)"
else
  echo "no video recorded" >&2
  exit 1
fi

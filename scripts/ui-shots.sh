#!/usr/bin/env bash
# Screenshots of the UI on synthetic frames, at the phone's 480x1040, in a
# headless Weston inside a Docker container: never on your own desktop.
# Writes target/ui-shots/*.png. Needs Docker; builds the app (no libcamera).
set -euo pipefail
cd "$(dirname "$0")/.."
IMAGE=omacam-uitest:1
if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
  docker build -t "$IMAGE" - <<'DOCKERFILE'
FROM archlinux:latest
RUN pacman -Syu --noconfirm --needed weston gtk4 libadwaita adwaita-icon-theme libjpeg-turbo ttf-dejavu mesa && pacman -Scc --noconfirm
DOCKERFILE
fi
cargo build --release --locked -p omacam
out=target/ui-shots
rm -rf "$out"; mkdir -p "$out"; chmod 777 "$out"
docker run --rm -v "$PWD/target/release/omacam:/omacam:ro" -v "$PWD/$out:/out" "$IMAGE" bash -c '
  export XDG_RUNTIME_DIR=/tmp/rt; mkdir -m 700 -p $XDG_RUNTIME_DIR
  weston --backend=headless --width=480 --height=1040 --socket=wayland-9 --idle-time=0 >/tmp/weston.log 2>&1 &
  sleep 2
  WAYLAND_DISPLAY=wayland-9 GSK_RENDERER=cairo timeout 60 /omacam --fake --debug --output-dir /out/photos --script "
    wait 2; shot /out/main-1x.png; zoom 2; wait 1; shot /out/main-2x.png;
    pinch 0.6; wait 1.5; shot /out/ultrawide.png; zoom 5; wait 1.5; shot /out/tele.png;
    shutter; wait 3; shot /out/after-photo.png; flip; wait 1.5; shot /out/front.png; quit"
' 2>&1 | grep -E "WARN|ERROR|window saved" || true
ls "$out"

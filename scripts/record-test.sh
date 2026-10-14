#!/usr/bin/env bash
# The recorder's tests with GStreamer's encoders present (x264enc from
# gst-plugins-ugly, openh264enc from -bad, vp8enc and mp4mux from -good),
# in a Docker container, so the encode-to-a-file test runs rather than
# skips. CODEC_PLUGINS="gst-plugins-good" leaves out the H.264 encoders to
# test the VP8/WebM fallback.
set -euo pipefail
cd "$(dirname "$0")/.."
plugins=${CODEC_PLUGINS:-gst-plugins-good gst-plugins-ugly gst-plugins-bad}
tag=omacam-gst-test:$(echo "$plugins" | sha256sum | cut -c1-8)
if ! docker image inspect "$tag" >/dev/null 2>&1; then
  docker build -t "$tag" - <<DOCKERFILE
FROM archlinux:latest
RUN pacman -Syu --noconfirm --needed rust gcc pkgconf gstreamer gst-plugins-base $plugins && pacman -Scc --noconfirm
DOCKERFILE
fi
mkdir -p "$HOME/.cargo/registry"
docker run --rm --user "$(id -u):$(id -g)" -v "$PWD:/src" -v "$HOME/.cargo/registry:/cargo/registry" \
  -e CARGO_HOME=/cargo -e HOME=/tmp -e CARGO_TARGET_DIR=/src/target/record-test -w /src "$tag" \
  cargo test --locked -p omacam-record -- --nocapture

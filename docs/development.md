# Developing omacam

How to build omacam, cross-compile it for a phone, and test it. The design
is in `docs/architecture.md`.

## Build on the machine that runs it

```
bin/build
```

`bin/build` runs `cargo build --release --locked -p omacam --features
libcamera`; the binary lands in `target/release/omacam`. It needs Rust,
clang (bindgen for `libcamera-sys`), pkgconf, and the libraries in the
`pkgbuild/PKGBUILD` `depends` with their headers: gtk4, libadwaita,
libcamera 0.7 and libjpeg-turbo. `bin/install` builds and installs the
pacman package.

Without libcamera (a laptop), `cargo build` builds the app without the real
backend, and `omacam --fake` runs it on synthetic frames.

## Cross-compile for the phone

Building Rust under qemu is slow, so `bin/cross` cross-compiles on an x86_64
machine instead: an x86_64 Rust with the aarch64 standard library, clang
and lld in a Docker container (Arch Linux), linking against an aarch64
sysroot copied out of an Arch Linux ARM container that has the phone's
libraries. It needs Docker and, for the sysroot step only, a qemu-aarch64
binfmt handler (`qemu-user-static-binfmt`).

```
bin/cross sysroot     # once, and after library updates (~2 GB in ~/.cache/omacam)
bin/cross build       # target/aarch64-unknown-linux-gnu/release/omacam
bin/cross package     # pkgbuild/omacam-<version>-1-aarch64.pkg.tar.zst
bin/cross check       # clippy for the libcamera backend
```

The first build takes about three minutes, later ones seconds.

The sysroot takes libcamera from Arch Linux ARM. To match a phone that runs
Omarchy Mobile's patched libcamera, put those packages in a directory and
pass it: `OMACAM_EXTRA_PKGS=~/omarchy-mobile/out/repo/aarch64 bin/cross
sysroot` installs every package in it, so use a directory with only the
`libcamera` and `libcamera-ipa` packages. The headers and the soname are the same,
so it matters only when a patch changes the API.

To try a build on a phone over ssh without packaging it:

```
ssh phone 'cat > /usr/local/bin/omacam && chmod 755 /usr/local/bin/omacam' \
  < target/aarch64-unknown-linux-gnu/release/omacam
```

## Testing

- **`scripts/check.sh`** runs `cargo fmt --check`, `cargo clippy
  --all-targets -- -D warnings` and `cargo test`; run it before every
  commit (`--fix` applies rustfmt's and clippy's fixes first). On a machine
  without libcamera it leaves out the backend; `bin/cross check` covers it.
- **Unit tests** cover the pure logic in `omacam-core`: the lens table and
  its matching, zoom thresholds and hysteresis, the slider mapping, the
  settle timer, the stream state machine (coalescing, stale frames, stop
  and resume, errors), the still sequence (autofocus wait, the discard
  rule, timeouts), exposure transfer, and the controller end to end. In the
  app: the viewfinder fit, crop and rotation of photos, JPEG encoding,
  file naming, and the test script parser.
- **`crates/omacam-fake/tests/controller.rs`** drives the controller through
  the fake backend in real time: zooming across all lenses, a full still,
  flipping, and a busy camera that frees up.

- **`scripts/record-test.sh`** runs the recorder's tests in a container
  that has GStreamer's encoders, so the encode-to-a-file test runs (it
  skips where they're missing); `CODEC_PLUGINS=gst-plugins-good` tests the
  VP8/WebM fallback.
- **`scripts/ui-shots.sh`** takes screenshots of the UI on synthetic
  frames at 480x1040 in a headless Weston inside a container (never on
  your own desktop), into `target/ui-shots/`.

### Logs

From a terminal omacam logs to stderr. Started from the app launcher,
where stderr goes nowhere, it logs to the journal: `journalctl -t omacam`
for its own lines (stream starts, validated configurations, buffer
allocations with their size, start failures with the failing step), and
libcamera's warnings and errors go to syslog too (`LIBCAMERA_LOG_FILE=syslog`,
`LIBCAMERA_LOG_LEVELS=*:WARN` unless set). `RUST_LOG=omacam=debug` adds
more.

### On a phone someone is using

The phone is someone's phone. Keep runs short, and leave the cameras
alone while another app uses them (omacam retries a busy camera every 2 s
rather than taking it).

- `omacam --list` lists the cameras and opens no stream.
- `--script` drives the app with no hands, and `--output-dir` keeps its
  photos out of `~/Pictures`:

  ```
  omacam --debug --output-dir /tmp/omacam-test --script \
    "wait 4; shot /tmp/omacam-test/main.png; pinch 0.6; wait 3; zoom 5; wait 3;
     flip; wait 3; flip; wait 3; shutter; wait 4; quit"
  ```

  Steps: `wait SECONDS`, `zoom Z` (a preset button), `pinch Z`, `flip`,
  `shutter`, `tap U V` (a tap on the picture), `settings PAGE`
  (photo, video, advanced), `close-settings`, `shot PATH` and `quit`. After each step the app logs a `state`
  line: lens, zoom, frame size, frame rate, the last switch time. `shot`
  renders the window to a PNG itself; with the panel off, when the
  compositor draws nothing, it renders the viewfinder alone.
- To run it in the user's session from a root shell on the phone:
  `systemd-run --machine=USER@.host --user --unit=omacam-test --collect -p
  RuntimeMaxSec=60 /usr/bin/omacam --debug --script "…"`, and read its log
  with `journalctl _SYSTEMD_USER_UNIT=omacam-test.service`.
- Screenshots of the whole screen, while the panel is on:
  `systemd-run --machine=USER@.host --user --pipe --wait timeout 10 grim -s
  0.5 - > shot.png`. They show the room through the camera: keep them
  private.

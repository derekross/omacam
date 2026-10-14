# omacam architecture and design

omacam is a phone camera on libcamera, GTK 4 and libadwaita. This document
covers what is built, the design choices behind it, and what is pending.
The design follows Omarchy Mobile's camera app plan
(`docs/CAMERA-APP-PLAN.md` in omarchy-mobile); section numbers below refer
to it. Building and testing are in `docs/development.md`.

## Crates

```
omacam-core        no UI, no camera library; unit-tested
  backend          the Backend trait, commands, events, frames
  lens             the lens table (TOML) matched to the cameras present
  zoom             the lens ladder, thresholds, hysteresis, slider maths
  switch           the stream state machine (stop one camera, start the next)
  capture          the still sequence (lock, reconfigure, discard, keep, restore)
  exposure         carrying exposure across a lens switch by aperture
  controller       ties them together: input and events in, commands out
omacam-libcamera   the backend on libcamera-rs, on its own thread
omacam-fake        a backend with synthetic frames, for tests and the laptop
omacam             the GTK app: viewfinder, controls, JPEG writing, theme
```

## Data flow

```
  UI thread (GTK)                                  backend thread
  ───────────────                                  ──────────────
  gestures, buttons ─► Controller ─► Command ───►  CameraManager, ActiveCamera
                          ▲                         requests, buffers
                          │ Event                       │
  Viewfinder ◄─ Show ─────┤ ◄── channel + frame slot ◄──┘ completed request:
  (GdkMemoryTexture       │                              copy (memory) or lend
   or GdkDmabufTexture)   └─ Picture ─► encoder thread ─► JPEG in ~/Pictures/Camera
```

libcamera-rs's `ActiveCamera` isn't `Send`, so the camera manager, the
camera and its buffers live on one backend thread. Commands, completed
requests and returned buffers reach it through one channel and are handled
in order. Every stream start has a generation number; frames and events
carry it, and the controller drops anything from a stream it has left.

Frames reach the UI through a one-frame slot: a new frame replaces an
unshown one, whose buffer goes back to the camera at once, so a slow UI
skips frames instead of queueing 9 MB copies.

## The cameras on gs201

- **One camera streams at a time** (all CSIS links share one write DMA
  context), so a lens switch is a stop and a start. The switch state
  machine keeps at most one start in flight; requests that arrive while a
  camera starts only replace the target, so a pinch through three lenses
  costs at most two starts. Measured on the Pixel 7 Pro, from the request to
  the first frame of the new camera: 280-420 ms (main, ultrawide, front),
  about 620 ms to the telephoto.
- **One processed stream.** libcamera's software ISP gives one processed
  stream, so a full-resolution still is a reconfiguration into the large
  mode (§3.3): wait for autofocus (at most 500 ms), note exposure and gains,
  start the still mode, drop at least two frames and at most six until the
  exposure is back, keep one, and start the preview again. The main camera's
  still mode is 4072x3072 at about 9 fps.
- **Formats.** omacam asks for XRGB8888 (32-bit linear, the safe GPU import
  format); libcamera adjusts the sizes slightly (2016x1136 becomes
  2008x1136).

## Zoom and lens switching (§3.4)

Zoom is relative to the main camera. The Pixel 7 Pro's factors come from
Google's published fields of view: ultrawide 0.45x, main 1x, telephoto
4.78x (`data/cheetah.toml`), not yet calibrated on our own frames.

A lens shows any zoom at or above its own factor by cropping its frame; it
can't show less. So a switch down to a wider lens happens as soon as the
zoom drops below the current lens's factor, and the 6 % hysteresis sits
above each threshold: from the main camera, the telephoto takes over at
4.78 × 1.06 = 5.07x during a pinch. (The plan puts the band below the
threshold, which a lens can't display.) A pinch or a slider drag switches
only after the zoom has rested for 150 ms; a preset button switches at once
and without hysteresis, so the 5 button lands on the telephoto.

While a new lens starts, the last frame stays on screen and the zoom keeps
applying to it, so the picture follows the fingers.

Zoom within a lens is a crop in the render transform (the software ISP
doesn't apply `ScalerCrop`); the photo applies the same crop to the
full-resolution frame.

## Display (§3.2)

Each frame becomes a texture drawn by the `Viewfinder` widget: rotated by
the camera's `Rotation` property (main 90°, ultrawide 270°, telephoto 270°,
front 90° on today's device tree), mirrored for the front camera, scaled by
the zoom crop, and fitted whole into the window.

Two paths, `--display`:

- **memory** (default): the backend copies the frame and requeues its buffer
  at once; the UI wraps the copy in a `GdkMemoryTexture`.
- **dmabuf**: the backend lends the buffer's dmabuf; the UI wraps it in a
  `GdkDmabufTexture` whose release callback returns the buffer. Before a
  stream stops, the frozen frame is downloaded into memory so every buffer
  is back before the allocator frees it. Works on the Pixel 7 Pro with Mesa
  26.2.4-1.2; it becomes the default once the panthor export fix is in the
  released Mesa package.

## Sharing the cameras

libcamera locks a camera per process. omacam releases every camera and its
camera manager when the window is suspended (GTK's toplevel `suspended`
state) or has been unfocused for 10 s, and opens them again when it comes
back. A camera that fails to start (another app has it) is retried every
2 s.

## Theme

`crates/omacam/src/theme.rs` follows the Omarchy theme as gliff does: it
reads `~/.local/state/omarchy/current/theme/colors.toml`, maps the palette
onto libadwaita's CSS variables, forces the light or dark scheme, and
watches the directory for theme changes. The viewfinder stays black; the
active zoom button and the busy shutter take the theme's accent.

## Not built yet

- Tap to focus and exposure compensation (need the libcamera patches L1,
  L2 in Omarchy Mobile's `packages/libcamera`).
- EXIF data, the shutter sound, location tags, the in-app viewer of recent
  shots, video (P6).
- Exposure carried across a lens switch: computed (`exposure.rs`), passed to
  libcamera only with `--exposure-hints` until it's checked whether the
  simple IPA takes `ExposureTime` as a starting point or as manual exposure.
- Calibration of the zoom factors and lens centre offsets (P4); the
  cross-fade at a switch.

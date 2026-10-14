# omacam

omacam is the camera app of Omarchy Mobile, shown as **Camera**. It is made
for phones: one zoom control runs from the ultrawide through the main camera
to the telephoto and picks the lens for you, as the stock Android camera
does, and photos come from the camera's full-resolution mode instead of the
preview.

It talks to the cameras through [libcamera](https://libcamera.org) directly,
not through PipeWire, so it can use what a webcam pipeline can't: every
camera of the phone, its modes and its controls. The window follows your
Omarchy theme.

The reference phone is the Google Pixel 7 Pro running
[Omarchy Mobile](https://github.com/derekross/omarchy-mobile). On other
machines omacam uses the first back camera and the first front camera it
finds.

## What you get

- A full-screen viewfinder, upright and mirrored for selfies.
- Zoom from 0.45x to 15x: the 0.5, 1, 2 and 5 buttons, a slider (drag
  sideways across the buttons), or a pinch anywhere on the picture. The
  ultrawide, main and telephoto cameras take over as you pass 1x and 4.78x.
- Photos from the full-resolution mode (12.5 MP from the main camera),
  cropped to the zoom and saved upright as JPEG in `~/Pictures/Camera`.
- Switching between the back and the front camera.
- Tap the picture to focus there (on cameras whose autofocus takes a
  window; otherwise the camera focuses on the centre, and a fixed-focus
  camera says so).
- The last photo as a thumbnail; tap it to open it.
- Video: 1080p or 720p at 30 fps, H.264 with Opus sound in MP4, saved in
  `~/Videos/Camera`, with zoom while recording (within the lens in use).
- Photos with EXIF data (time to the millisecond, exposure, ISO, aperture,
  focal length, which camera, zoom; no serial numbers, no location) and
  the shutter sound. Photos and videos follow how the phone is held.
- Settings from the gear: aspect ratio (full, 4:3, 16:9, 1:1), resolution,
  JPEG quality, a 3 or 10 s timer, a 3x3 or 4x4 grid, mirrored selfies,
  the shutter sound, where photos are saved, volume keys as the shutter,
  tap to focus, and the debug overlay. They are kept in
  `~/.config/omacam/settings.toml`.
- The cameras are let go while the window is hidden, so other apps (a
  browser, a video call) can use them.

Not yet: exposure compensation, lens switching while recording, other
video frame rates, location tags, RAW, a level, HDR and night modes. See
[docs/architecture.md](docs/architecture.md) for the plan.

## Install

Once omacam is in Omarchy Mobile's package repository, install the
`omacam` package. To build and install the package from a checkout on the
phone itself:

```
bin/install
```

To build it on an x86_64 laptop for the phone, see
[docs/development.md](docs/development.md) (`bin/cross package`).

## Use

Open **Camera** from the app launcher, or run `omacam`.

```
omacam --list            # the cameras and the role each one plays
omacam --front           # open on the front camera
omacam --zoom 2          # open at 2x
omacam --capture quick   # photos from the preview, without the mode switch
omacam --remote shutter  # take a photo with the running omacam
```

Volume keys take photos when they reach the window. On Omarchy Mobile the
shell binds them for the system volume, so it has to hand them over with
`omacam --remote shutter` while the camera is in front.

`--display dmabuf` shows the camera's buffers without copying them, which
saves CPU and battery; it needs a Mesa with the panthor export fix, and
becomes the default once that ships. `--debug` shows the frame rate, the
lens and the lens switch time over the picture.

A lens table describes the cameras of a phone: which is which, how much
each one zooms, and which modes to use. omacam has the Pixel 7 Pro's built
in (`data/cheetah.toml`); `~/.config/omacam/lenses.toml` or
`--lens-table FILE` replaces it.

## Picture quality

omacam uses libcamera's software ISP. There is no HDR+ and no Google
tuning, so pictures are not as good as the stock camera's. The telephoto's
focus motor doesn't work yet on the Pixel 7 Pro, so its pictures are soft.

## More

- [docs/architecture.md](docs/architecture.md): how it works and why.
- [docs/development.md](docs/development.md): building, cross-compiling for
  the phone, testing.

omacam is released under the GNU General Public License, version 3 or
later. Its theme support is adapted from
[gliff](https://github.com/omacom/gliff) (MIT); see NOTICE.

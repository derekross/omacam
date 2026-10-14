# omacam

Run `scripts/check.sh` before every commit. It runs rustfmt, clippy with
warnings as errors, and the unit tests; a clippy warning is a failure. The
libcamera backend needs libcamera to build, so on a machine without it run
`bin/cross check` too. See `docs/architecture.md` for the design and
`docs/development.md` for the build, the cross-compile for the phone, and
how to test on a phone that someone is using (`--script`, `--output-dir`).

The camera logic lives in `crates/omacam-core` and has no UI and no
libcamera: test it there, with `crates/omacam-fake` for the backend.

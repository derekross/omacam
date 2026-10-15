#!/usr/bin/env bash
# The checks to run before every commit: formatting, clippy with warnings
# as errors, and the unit tests. The libcamera backend needs libcamera to
# build, so on a machine without it these cover the rest of the workspace
# and `bin/cross check` covers the backend (see docs/development.md).
# Pass --fix to let rustfmt and clippy apply their fixes first.
set -euo pipefail
cd "$(dirname "$0")/.."

all=()
if pkg-config --exists libcamera 2>/dev/null; then
    all=(--workspace --features omacam/libcamera)
fi

if [ "${1:-}" = "--fix" ]; then
    cargo fmt --all
    cargo clippy --fix "${all[@]}" --all-targets --allow-dirty --allow-staged --locked
fi

echo "== spelling (US English)"
# British spellings, except in libcamera's own control names (ColourGains,
# ColourTemperature, AnalogueGain, CentreWeighted), which stay as libcamera
# spells them.
british='colour|centre|centred|enterd|licence|\bgrey|cancelled|metre|analogue|behaviour|favourite|honour|neighbour|(normal|real|recogn|initial|optim|organ|minim|maxim|stabil|sanit|serial|summar|visual|prior|synchron|categor|emphas|special|util|custom|author|capital|standard)is(e|es|ed|ing|ation|ations)\b|judgement|artefact|whilst|catalogue'
api='Colour(Gains|Temperature|CorrectionMatrix)|AnalogueGain|CentreWeighted'
if git ls-files -z -- ':!*.png' ':!*.jpg' ':!Cargo.lock' ':!scripts/check.sh' \
    | xargs -0 grep -nIiE "$british" \
    | grep -vE "$api" ; then
    echo "British spellings above: omacam is written in US English" >&2
    exit 1
fi
echo "== fmt"
cargo fmt --all --check
echo "== clippy"
cargo clippy "${all[@]}" --all-targets --locked -- -D warnings
echo "== test"
cargo test "${all[@]}" --locked
echo "CHECK PASS"

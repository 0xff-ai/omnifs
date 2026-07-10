#!/usr/bin/env bash
set -euo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

# The release image is the top-level Dockerfile's `runtime-release` target: it
# shares the `runtime-base` stage with the contributor image, and the prebuilt
# binary is injected as the `omnifs-bin` named build context rather than copied
# from a compile stage. Targeting `runtime-release` builds only
# `ubuntu -> runtime-base -> runtime-release`, so the toolchain never runs.
IMAGE="${IMAGE:-omnifs:native-runtime}"

build_release_stage_image runtime-release

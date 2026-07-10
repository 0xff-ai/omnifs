#!/usr/bin/env bash
set -euo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

# The frontend image is the top-level Dockerfile's `frontend-release` target:
# a minimal Debian userland plus the prebuilt `omnifs` binary, injected as the
# `omnifs-bin` named build context.
IMAGE="${IMAGE:-omnifs-frontend:native}"

build_release_stage_image frontend-release

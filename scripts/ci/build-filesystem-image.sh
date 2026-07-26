#!/usr/bin/env bash
# Build the slim filesystem image.
set -euo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

# The filesystem image is the top-level Dockerfile's `filesystem-release` target:
# a minimal Debian userland plus the prebuilt slim `omnifs-thin` binary,
# injected as the `omnifs-thin-bin` named build context.
IMAGE="${IMAGE:-omnifs-filesystem:native}"

build_release_stage_image filesystem-release

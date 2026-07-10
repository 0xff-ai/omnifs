# Helpers sourced by scripts/ci/*.sh. Source with:
#
#   source "$(dirname "${BASH_SOURCE[0]}")/common.sh"
#
# Sets $root to the repo root.

# shellcheck disable=SC2034
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

# Build one of the top-level Dockerfile's release-style final stages for one
# platform: a minimal image with a prebuilt Linux `omnifs` binary injected as
# the `omnifs-bin` build context, never compiling inside Docker. Shared by
# build-runtime-image.sh and build-frontend-image.sh so the buildx mechanics
# have one owner; only the target stage, image default, and launcher-version
# label differ between the two callers.
#
# $1: the Dockerfile stage to build (`runtime-release` or `frontend-release`).
# $2: "true" to bake OMNIFS_MIN_LAUNCHER_VERSION as a build arg (the runtime
#     image's launcher-compat label; the frontend image carries no such label,
#     since `launch_frontend_container` never checks one), default "true".
#
# Reads IMAGE (required), PLATFORM, PUSH, METADATA_FILE, OMNIFS_BINARY,
# OMNIFS_LINUX_TARGET, OMNIFS_MIN_LAUNCHER_VERSION from the environment (all
# optional beyond IMAGE). Writes image/platform/digest to $GITHUB_OUTPUT when
# set, and to stdout unconditionally.
build_release_stage_image() {
  local dockerfile_target="$1"
  local with_launcher_version="${2:-true}"

  local image="${IMAGE:?IMAGE must be set}"
  local platform="${PLATFORM:-}"
  local push="${PUSH:-false}"
  local metadata_file="${METADATA_FILE:-}"

  if [[ -z "$platform" ]]; then
    local arch
    arch="$(docker version --format '{{.Server.Arch}}')"
    case "$arch" in
      amd64 | x86_64) platform="linux/amd64" ;;
      arm64 | aarch64) platform="linux/arm64" ;;
      *) echo "unsupported docker server arch: $arch" >&2; return 1 ;;
    esac
  fi

  local target
  case "$platform" in
    linux/amd64) target="${OMNIFS_LINUX_TARGET:-x86_64-unknown-linux-gnu}" ;;
    linux/arm64) target="${OMNIFS_LINUX_TARGET:-aarch64-unknown-linux-gnu}" ;;
    *) echo "unsupported runtime platform: $platform" >&2; return 1 ;;
  esac

  local binary="${OMNIFS_BINARY:-$root/target/$target/release/omnifs}"
  if [[ ! -x "$binary" ]]; then
    echo "missing native Linux binary: $binary" >&2
    return 1
  fi

  local bindir
  bindir="$(mktemp -d)"
  trap 'rm -rf "$bindir"' RETURN

  cp "$binary" "$bindir/omnifs"

  if [[ -z "$metadata_file" ]]; then
    metadata_file="$bindir/build-metadata.json"
  fi

  local output_arg=(--load)
  if [[ "$push" == "true" ]]; then
    output_arg=(--push)
  fi

  local build_args=()
  if [[ "$with_launcher_version" == "true" ]]; then
    if [[ -z "${OMNIFS_MIN_LAUNCHER_VERSION:-}" ]]; then
      OMNIFS_MIN_LAUNCHER_VERSION="$(awk -F'"' '/^version = / {print $2; exit}' "$root/Cargo.toml")"
    fi
    build_args+=(--build-arg "OMNIFS_MIN_LAUNCHER_VERSION=${OMNIFS_MIN_LAUNCHER_VERSION}")
  fi

  docker buildx build "${output_arg[@]}" \
    --metadata-file "$metadata_file" \
    --platform "$platform" \
    --target "$dockerfile_target" \
    --build-context "omnifs-bin=$bindir" \
    "${build_args[@]}" \
    -t "$image" \
    -f "$root/Dockerfile" \
    "$root"

  local digest=""
  if [[ -s "$metadata_file" ]]; then
    digest="$(jq -r '."containerimage.digest" // empty' "$metadata_file")"
  fi

  if [[ -n "${GITHUB_OUTPUT:-}" ]]; then
    {
      echo "image=$image"
      echo "platform=$platform"
      echo "digest=$digest"
    } >>"$GITHUB_OUTPUT"
  fi

  echo "image=$image"
  echo "platform=$platform"
  echo "digest=$digest"
  echo "binary=$binary"
}

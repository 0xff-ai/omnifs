#!/usr/bin/env bash
set -euo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

if [[ $# -ne 1 ]]; then
  echo "usage: scripts/ci/build-libkrun-runtime.sh OUT_DIR" >&2
  exit 2
fi

if [[ "$(uname -s)" != "Darwin" || "$(uname -m)" != "arm64" ]]; then
  echo "the packaged libkrun runtime can be built only on macOS arm64" >&2
  exit 1
fi

readonly libkrun_repository="https://github.com/containers/libkrun.git"
readonly libkrun_revision="728df8125077d0db44265f6e997c72b81b65c015"
readonly libkrun_version="1.19.4"
readonly firmware_repository="https://github.com/slp/edk2"
readonly firmware_revision="13e8adac8a83141b51375c799996946082e1eb43"
readonly firmware_sha256="9ba725c245f634c86d9cc0850ddcb60b7efe05c6abb53f7ebabf9cd0b070d3de"

out_dir="$(mkdir -p "$1" && cd "$1" && pwd)"
build_dir="$(mktemp -d)"
cleanup() {
  rm -rf "$build_dir"
}
trap cleanup EXIT

source_dir="$build_dir/libkrun"
git init -q "$source_dir"
git -C "$source_dir" remote add origin "$libkrun_repository"
git -C "$source_dir" fetch --depth 1 origin "$libkrun_revision"
git -C "$source_dir" checkout -q --detach FETCH_HEAD
actual_revision="$(git -C "$source_dir" rev-parse HEAD)"
if [[ "$actual_revision" != "$libkrun_revision" ]]; then
  echo "libkrun revision mismatch: expected $libkrun_revision, got $actual_revision" >&2
  exit 1
fi

firmware_source="$source_dir/src/vmm/edk2/KRUN_EFI.silent.fd"
actual_firmware_sha256="$(shasum -a 256 "$firmware_source" | awk '{print $1}')"
if [[ "$actual_firmware_sha256" != "$firmware_sha256" ]]; then
  echo "firmware hash mismatch: expected $firmware_sha256, got $actual_firmware_sha256" >&2
  exit 1
fi
if ! grep -Fq "$firmware_revision" "$source_dir/src/vmm/edk2/Sources.txt"; then
  echo "libkrun firmware provenance does not name EDK2 revision $firmware_revision" >&2
  exit 1
fi

libkrun_target="$build_dir/target"
(
  cd "$source_dir"
  CARGO_TARGET_DIR="$libkrun_target" \
    cargo build --locked --release --no-default-features --features efi -p libkrun
)

runtime_dir="$out_dir/libexec/omnifs"
licenses_dir="$runtime_dir/licenses"
rm -rf "$out_dir/libexec"
mkdir -p "$licenses_dir/libkrun" "$licenses_dir/edk2"

dylib="$runtime_dir/libkrun.1.dylib"
firmware="$runtime_dir/KRUN_EFI.silent.fd"
cp "$libkrun_target/release/libkrun.dylib" "$dylib"
cp "$firmware_source" "$firmware"
cp "$source_dir/LICENSE" "$licenses_dir/libkrun/LICENSE"
cp "$source_dir/src/vmm/edk2/License.txt" "$licenses_dir/edk2/License.txt"
cp "$source_dir/src/vmm/edk2/Sources.txt" "$licenses_dir/edk2/Sources.txt"
chmod 0755 "$dylib"
chmod 0644 "$firmware" "$licenses_dir/libkrun/LICENSE" "$licenses_dir/edk2/"*

install_name_tool -id "@rpath/libkrun.1.dylib" "$dylib"

libkrun_sha256="$(shasum -a 256 "$dylib" | awk '{print $1}')"
manifest="$runtime_dir/runtime-manifest.json"
printf '%s\n' \
  '{' \
  '  "schema": 1,' \
  '  "libkrun": {' \
  "    \"version\": \"$libkrun_version\"," \
  "    \"repository\": \"$libkrun_repository\"," \
  "    \"revision\": \"$libkrun_revision\"," \
  '    "features": ["blk", "efi", "net"],' \
  '    "disabled_features": ["gpu", "init-blob", "input", "snd"],' \
  "    \"pre_sign_sha256\": \"$libkrun_sha256\"" \
  '  },' \
  '  "firmware": {' \
  "    \"repository\": \"$firmware_repository\"," \
  "    \"revision\": \"$firmware_revision\"," \
  "    \"sha256\": \"$firmware_sha256\"" \
  '  }' \
  '}' >"$manifest"
chmod 0644 "$manifest"

"$root/scripts/ci/check-libkrun-runtime.sh" "$out_dir"
echo "libkrun runtime staged at $out_dir"

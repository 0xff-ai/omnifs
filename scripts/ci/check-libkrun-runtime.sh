#!/usr/bin/env bash
set -euo pipefail

signed=false
if [[ "${1:-}" == "--signed" ]]; then
  signed=true
  shift
fi
if [[ $# -ne 1 ]]; then
  echo "usage: scripts/ci/check-libkrun-runtime.sh [--signed] RUNTIME_ROOT" >&2
  exit 2
fi

root="$1"
runtime="$root/libexec/omnifs"
dylib="$runtime/libkrun.1.dylib"
firmware="$runtime/KRUN_EFI.silent.fd"
manifest="$runtime/runtime-manifest.json"

for file in \
  "$dylib" \
  "$firmware" \
  "$manifest" \
  "$runtime/licenses/libkrun/LICENSE" \
  "$runtime/licenses/edk2/License.txt" \
  "$runtime/licenses/edk2/Sources.txt"; do
  if [[ ! -f "$file" ]]; then
    echo "missing packaged libkrun runtime file: $file" >&2
    exit 1
  fi
done

if [[ "$(stat -f '%Lp' "$dylib")" != "755" ]]; then
  echo "packaged libkrun dylib must have mode 0755" >&2
  exit 1
fi
for file in "$firmware" "$manifest" "$runtime/licenses/libkrun/LICENSE" "$runtime/licenses/edk2/"*; do
  if [[ "$(stat -f '%Lp' "$file")" != "644" ]]; then
    echo "packaged runtime data must have mode 0644: $file" >&2
    exit 1
  fi
done

if [[ "$(lipo -archs "$dylib")" != "arm64" ]]; then
  echo "packaged libkrun dylib is not arm64-only: $(lipo -archs "$dylib")" >&2
  exit 1
fi
if [[ "$(otool -D "$dylib" | tail -n 1)" != "@rpath/libkrun.1.dylib" ]]; then
  echo "packaged libkrun dylib has an unexpected install name" >&2
  otool -D "$dylib" >&2
  exit 1
fi

links="$(otool -L "$dylib")"
if grep -Eiq '(/opt/homebrew|/usr/local|libepoxy|virglrenderer)' <<<"$links"; then
  echo "packaged libkrun dylib has a forbidden dependency:" >&2
  printf '%s\n' "$links" >&2
  exit 1
fi

symbols="$(nm -gU "$dylib" | awk '{print $NF}')"
for symbol in \
  krun_init_log \
  krun_create_ctx \
  krun_free_ctx \
  krun_has_feature \
  krun_set_firmware \
  krun_set_vm_config \
  krun_add_disk3 \
  krun_disable_implicit_vsock \
  krun_add_vsock \
  krun_add_vsock_port2 \
  krun_set_console_output \
  krun_get_shutdown_eventfd \
  krun_start_enter; do
  if ! grep -qx "_$symbol" <<<"$symbols"; then
    echo "packaged libkrun dylib is missing required symbol $symbol" >&2
    exit 1
  fi
done

expected_firmware_sha256="$(jq -er '.firmware.sha256' "$manifest")"
actual_firmware_sha256="$(shasum -a 256 "$firmware" | awk '{print $1}')"
if [[ "$actual_firmware_sha256" != "$expected_firmware_sha256" ]]; then
  echo "packaged firmware hash does not match runtime-manifest.json" >&2
  exit 1
fi

expected_libkrun_sha256="$(jq -er '.libkrun.pre_sign_sha256 | select(test("^[0-9a-f]{64}$"))' "$manifest")"
if [[ "$signed" == "false" ]]; then
  actual_libkrun_sha256="$(shasum -a 256 "$dylib" | awk '{print $1}')"
  if [[ "$actual_libkrun_sha256" != "$expected_libkrun_sha256" ]]; then
    echo "packaged libkrun hash does not match runtime-manifest.json" >&2
    exit 1
  fi
fi

jq -e '
  .schema == 1 and
  .libkrun.version == "1.19.4" and
  .libkrun.repository == "https://github.com/containers/libkrun.git" and
  .libkrun.revision == "728df8125077d0db44265f6e997c72b81b65c015" and
  (.libkrun.features | sort) == ["blk", "efi", "net"] and
  (.libkrun.disabled_features | sort) == ["gpu", "init-blob", "input", "snd"] and
  .firmware.repository == "https://github.com/slp/edk2" and
  .firmware.revision == "13e8adac8a83141b51375c799996946082e1eb43" and
  .firmware.sha256 == "9ba725c245f634c86d9cc0850ddcb60b7efe05c6abb53f7ebabf9cd0b070d3de"
' "$manifest" >/dev/null

echo "libkrun runtime audit passed"

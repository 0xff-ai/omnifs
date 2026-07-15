#!/usr/bin/env bash
# Boot the libkrun guest image once, with a throwaway seed ISO carrying
# placeholder (unreachable) attach parameters, and check the serial console
# log for two things: the guest reaching multi-user (systemd/EFI boot
# actually worked) and the omnifs-frontend.service runner starting (the seed
# was found, mounted, and the unit execed the binary). A successful attach is
# not expected: the placeholder address has no route from inside the guest,
# so `omnifs-thin fuse` retries its connect for up to 30s
# (INITIAL_CONNECT_DEADLINE in crates/omnifs-vfs-wire/src/client.rs)
# before giving up. This smoke intentionally covers guest boot and service
# startup only; it does not exercise a live attach.
#
# Requires target/guest-image/omnifs-guest.raw (`just guest-image`) and
# the krunkit executable on PATH.
set -uo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
image="${GUEST_IMAGE:-$root/target/guest-image/omnifs-guest.raw}"
boot_timeout="${BOOT_TIMEOUT_SECONDS:-90}"

if [[ ! -f "$image" ]]; then
  echo "missing guest image: $image (run \`just guest-image\` first)" >&2
  exit 1
fi
if ! command -v krunkit >/dev/null 2>&1; then
  echo "krunkit not found on PATH" >&2
  exit 1
fi

work="$(mktemp -d)"
root_image="$work/root.raw"
seed_iso="$work/seed.iso"
serial_log="$work/serial.log"

image_hash_before="$(shasum -a 256 "$image" | awk '{print $1}')"
image_mode_before="$(stat -f '%Lp' "$image")"
cp "$image" "$root_image"
chmod 600 "$root_image"
if [[ "$(stat -f '%Lp' "$root_image")" != "600" ]]; then
  echo "writable root copy did not have mode 0600" >&2
  exit 1
fi

# TEST-NET-1 (RFC 5737): guaranteed non-routable from a guest with no network
# device attached, so the connect fails/keeps retrying instead of accidentally
# reaching something real.
"$root/scripts/guest-image/make-seed-iso.sh" \
  --out "$seed_iso" \
  --attach-addr "192.0.2.1:9999" \
  --attach-token "smoke-placeholder-token" \
  || exit 1

krunkit_pid=""

cleanup() {
  if [[ -n "$krunkit_pid" ]] && kill -0 "$krunkit_pid" 2>/dev/null; then
    kill "$krunkit_pid" 2>/dev/null
    for ((attempt = 0; attempt < 20; attempt++)); do
      kill -0 "$krunkit_pid" 2>/dev/null || break
      sleep 0.25
    done
    kill -9 "$krunkit_pid" 2>/dev/null || true
  fi
  image_hash_after="$(shasum -a 256 "$image" | awk '{print $1}')"
  image_mode_after="$(stat -f '%Lp' "$image")"
  if [[ "$image_hash_after" != "$image_hash_before" || "$image_mode_after" != "$image_mode_before" ]]; then
    echo "FAIL: immutable guest image changed during smoke" >&2
    exit 1
  fi
  rm -rf "$work"
}
trap cleanup EXIT

strip_ansi() {
  sed -E 's/\x1b\[[0-9;]*[a-zA-Z]//g' "$serial_log" 2>/dev/null
}

start_epoch=$(date +%s)

krunkit \
  --cpus 2 \
  --memory 2048 \
  --device "virtio-blk,path=${root_image},format=raw" \
  --device "virtio-blk,path=${seed_iso},format=raw" \
  --device "virtio-serial,logFilePath=${serial_log}" \
  --restful-uri none:// \
  >"$work/krunkit.stdout" 2>&1 &
krunkit_pid=$!

echo "libkrun launched (pid $krunkit_pid), serial log: $serial_log"

reached_multi_user=""
runner_started=""
deadline=$((start_epoch + boot_timeout))

while [[ $(date +%s) -lt $deadline ]]; do
  if ! kill -0 "$krunkit_pid" 2>/dev/null; then
    echo "libkrun executable exited early (pid $krunkit_pid); see $work/krunkit.stdout" >&2
    break
  fi
  # The console log interleaves ANSI color codes into status lines (e.g. an
  # escape sequence lands mid-word inside "multi-user.target"), so strip them
  # before matching instead of trying to pattern-match around them.
  stripped="$(strip_ansi || true)"
  if [[ -z "$reached_multi_user" ]] && grep -qiE "Reached target multi-user\.target" <<<"$stripped"; then
    reached_multi_user="$(date +%s)"
  fi
  if [[ -z "$runner_started" ]] && grep -qiE "(Starting|Started) omnifs-frontend\.service" <<<"$stripped"; then
    runner_started="$(date +%s)"
  fi
  if [[ -n "$reached_multi_user" && -n "$runner_started" ]]; then
    break
  fi
  sleep 1
done

echo "== serial log tail (ANSI stripped) =="
strip_ansi | tail -n 80

status=0
if [[ -z "$reached_multi_user" ]]; then
  echo "FAIL: never saw the guest reach multi-user.target within ${boot_timeout}s" >&2
  status=1
else
  echo "PASS: reached multi-user.target at +$((reached_multi_user - start_epoch))s"
fi
if [[ -z "$runner_started" ]]; then
  echo "FAIL: never saw omnifs-frontend.service start within ${boot_timeout}s" >&2
  status=1
else
  echo "PASS: omnifs-frontend.service started at +$((runner_started - start_epoch))s"
fi

exit "$status"

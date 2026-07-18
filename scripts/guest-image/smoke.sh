#!/usr/bin/env bash
# Boot the libkrun guest image once through the packaged Omnifs helper, with a
# throwaway seed ISO carrying placeholder attach parameters, and check the serial console
# log for two things: the guest reaching multi-user (systemd/EFI boot
# actually worked) and the omnifs-frontend.service runner starting (the seed
# was found, mounted, and the unit execed the binary). A successful attach is
# not expected: no daemon listens on the mapped host socket, so
# `omnifs-thin fuse` retries its connect for up to 30s
# (INITIAL_CONNECT_DEADLINE in crates/omnifs-vfs-wire/src/client.rs)
# before giving up. This smoke intentionally covers guest boot and service
# startup only; it does not exercise a live attach.
#
# Requires target/guest-image/omnifs-guest.raw (`just guest-image`) and the
# staged helper payload under target/debug (`just libkrun-runtime`).
set -uo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
image="${GUEST_IMAGE:-$root/target/guest-image/omnifs-guest.raw}"
boot_timeout="${BOOT_TIMEOUT_SECONDS:-90}"
payload_root="${OMNIFS_LIBKRUN_PAYLOAD:-$root/target/debug}"
helper="$payload_root/omnifs-libkrun"
libkrun="$payload_root/libexec/omnifs/libkrun.1.dylib"
firmware="$payload_root/libexec/omnifs/KRUN_EFI.silent.fd"

if [[ ! -f "$image" ]]; then
  echo "missing guest image: $image (run \`just guest-image\` first)" >&2
  exit 1
fi
for file in "$helper" "$libkrun" "$firmware"; do
  if [[ ! -f "$file" ]]; then
    echo "missing packaged libkrun payload file: $file (run \`just libkrun-runtime\`)" >&2
    exit 1
  fi
done

work="$(mktemp -d)"
root_image="$work/root.raw"
seed_iso="$work/seed.iso"
serial_log="$work/serial.log"
diagnostic_log="$work/helper.log"
pid_file="$work/libkrun.pid"
control_socket="$work/control.sock"
attach_socket="$work/daemon-attach.sock"
attach_bridge_socket="$work/attach.sock"
ready_socket="$work/ready.sock"
ssh_socket="$work/ssh.sock"

image_hash_before="$(shasum -a 256 "$image" | awk '{print $1}')"
image_mode_before="$(stat -f '%Lp' "$image")"
cp "$image" "$root_image"
chmod 600 "$root_image"
if [[ "$(stat -f '%Lp' "$root_image")" != "600" ]]; then
  echo "writable root copy did not have mode 0600" >&2
  exit 1
fi

"$root/scripts/guest-image/make-seed-iso.sh" \
  --out "$seed_iso" \
  --attach-addr "vsock:1024" \
  --attach-token "smoke-placeholder-token" \
  || exit 1

helper_pid=""

cleanup() {
  if [[ -n "$helper_pid" ]] && kill -0 "$helper_pid" 2>/dev/null; then
    kill "$helper_pid" 2>/dev/null
    for ((attempt = 0; attempt < 20; attempt++)); do
      kill -0 "$helper_pid" 2>/dev/null || break
      sleep 0.25
    done
    kill -9 "$helper_pid" 2>/dev/null || true
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

dump_helper_logs() {
  for log in "$work/helper.stdout" "$diagnostic_log"; do
    if [[ -s "$log" ]]; then
      echo "== $(basename "$log") tail =="
      tail -n 80 "$log"
    fi
  done
}

start_epoch=$(date +%s)

"$helper" \
  --root-disk "$root_image" \
  --seed-disk "$seed_iso" \
  --serial-log "$serial_log" \
  --diagnostic-log "$diagnostic_log" \
  --pid-file "$pid_file" \
  --control-socket "$control_socket" \
  --attach-socket "$attach_socket" \
  --attach-bridge-socket "$attach_bridge_socket" \
  --readiness-socket "$ready_socket" \
  --ssh-socket "$ssh_socket" \
  --libkrun "$libkrun" \
  --firmware "$firmware" \
  --attach-port 1024 \
  --readiness-port 1025 \
  --ssh-port 22 \
  --resources 2:2048 \
  >"$work/helper.stdout" 2>&1 &
helper_pid=$!

echo "libkrun launched (pid $helper_pid), serial log: $serial_log"

reached_multi_user=""
runner_started=""
deadline=$((start_epoch + boot_timeout))

while [[ $(date +%s) -lt $deadline ]]; do
  if ! kill -0 "$helper_pid" 2>/dev/null; then
    echo "omnifs-libkrun exited early (pid $helper_pid)" >&2
    dump_helper_logs >&2
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

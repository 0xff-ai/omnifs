#!/usr/bin/env bash
# Static, fail-closed assertions against a built libkrun guest disk image.
# The privileged container makes the loop-mount checks portable to macOS.
# Usage: check-guest-image.sh IMAGE_PATH PROFILE
set -euo pipefail

image_path="${1:?usage: check-guest-image.sh IMAGE_PATH PROFILE}"
profile="${2:?usage: check-guest-image.sh IMAGE_PATH PROFILE}"

case "$profile" in
  dev | release) ;;
  *)
    echo "check-guest-image.sh: PROFILE must be dev or release, got: $profile" >&2
    exit 2
    ;;
esac

if [[ ! -f "$image_path" ]]; then
  echo "check-guest-image.sh: no such image: $image_path" >&2
  exit 1
fi

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

raw_path="$image_path"
if [[ "$image_path" == *.zst ]]; then
  echo "== decompressing $image_path =="
  raw_path="$work/$(basename "${image_path%.zst}")"
  zstd -d -f -q "$image_path" -o "$raw_path"
fi

raw_dir="$(cd "$(dirname "$raw_path")" && pwd)"
raw_name="$(basename "$raw_path")"

echo "== asserting guest image ($profile profile): $raw_path =="
docker run --rm -i --privileged \
  -v "$raw_dir:/img:ro" \
  -e "RAW_NAME=$raw_name" \
  -e "PROFILE=$profile" \
  debian:trixie-slim \
  bash -s <<'INNER'
set -euo pipefail

fail=0
note() { echo "-- $*"; }
violation() {
  echo "FAIL: $*" >&2
  fail=1
}

esp_loop=""
root_loop=""
cleanup() {
  umount /mnt/root 2>/dev/null || true
  umount /mnt/esp 2>/dev/null || true
  [[ -z "$root_loop" ]] || losetup -d "$root_loop" 2>/dev/null || true
  [[ -z "$esp_loop" ]] || losetup -d "$esp_loop" 2>/dev/null || true
}
trap cleanup EXIT

partition_loop() {
  local number="$1"
  local geometry start sectors extra
  geometry="$(
    partx --raw --noheadings --nr "$number" \
      --output START,SECTORS --sector-size 512 "/img/${RAW_NAME}"
  )"
  if ! read -r start sectors extra <<<"$geometry" ||
    [[ ! $start =~ ^[0-9]+$ || ! $sectors =~ ^[1-9][0-9]*$ || -n $extra ]]; then
    echo "invalid geometry for partition $number: ${geometry:-<missing>}" >&2
    partx --show "/img/${RAW_NAME}" >&2 || true
    return 1
  fi
  losetup --find --show --read-only \
    --offset "$((start * 512))" \
    --sizelimit "$((sectors * 512))" \
    "/img/${RAW_NAME}"
}

# Do not rely on the host kernel to materialize partition child devices.
# Privileged CI containers share the runner's block-device namespace, and some
# runners expose unrelated loop partition children. Parse the image's GPT and
# attach each partition as its own read-only loop instead.
esp_loop="$(partition_loop 1)"
root_loop="$(partition_loop 2)"

mkdir -p /mnt/root /mnt/esp
mount -o ro "$root_loop" /mnt/root
mount -o ro "$esp_loop" /mnt/esp

note "checking /usr/local/bin/omnifs-thin"
bin=/mnt/root/usr/local/bin/omnifs-thin
if [[ ! -f "$bin" ]]; then
  violation "missing $bin"
elif [[ ! -x "$bin" ]]; then
  violation "$bin is present but not executable"
fi

note "checking omnifs unit presence"
unit_dir=/mnt/root/etc/systemd/system
present_units=(
  omnifs-seed-mount.service
  omnifs-frontend.service
  omnifs-ssh-setup.service
  omnifs-dropbear.service
  omnifs-ssh.service
  omnifs-ssh.socket
)
for unit in "${present_units[@]}"; do
  if [[ ! -f "$unit_dir/$unit" ]]; then
    violation "missing unit file $unit"
  fi
done

note "checking omnifs unit enablement"
enabled_units=(omnifs-seed-mount.service omnifs-frontend.service omnifs-ssh-setup.service)
for unit in "${enabled_units[@]}"; do
  link="$unit_dir/multi-user.target.wants/$unit"
  if [[ ! -L "$link" ]]; then
    violation "$unit is not enabled (missing $link)"
  fi
done

note "checking for cloud-init"
if [[ -d /mnt/root/etc/cloud ]]; then
  violation "found /etc/cloud; this guest must never carry cloud-init"
fi
if find /mnt/root -iname '*cloud-init*' -print -quit 2>/dev/null | grep -q .; then
  violation "found a cloud-init-named path in the image"
fi

if [[ "$PROFILE" == "release" ]]; then
  note "checking root is locked (release profile)"
  shadow_line="$(grep '^root:' /mnt/root/etc/shadow || true)"
  root_field="$(echo "$shadow_line" | cut -d: -f2)"
  case "$root_field" in
    '*' | '!' | '!'*) ;;
    *)
      violation "root's /etc/shadow password field is not locked: '$shadow_line'"
      ;;
  esac

  note "checking for autologin drop-ins (release profile)"
  for unit in console-getty.service getty@tty1.service serial-getty@hvc0.service; do
    dropin="/mnt/root/usr/lib/systemd/system/${unit}.d/autologin.conf"
    if [[ -f "$dropin" ]]; then
      violation "found autologin drop-in for $unit; release profile must not autologin"
    fi
  done
fi

if [[ "$fail" -ne 0 ]]; then
  echo "one or more assertions failed for the $PROFILE profile" >&2
  exit 1
fi
echo "PASS: all $PROFILE profile assertions held"
INNER

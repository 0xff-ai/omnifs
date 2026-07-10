#!/usr/bin/env bash
# Static, fail-closed assertions against a built krunkit guest disk image.
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

loopdev="$(losetup -fP --show "/img/${RAW_NAME}")"
base="$(basename "$loopdev")"
cleanup() {
  umount /mnt/root 2>/dev/null || true
  umount /mnt/esp 2>/dev/null || true
  losetup -d "$loopdev" 2>/dev/null || true
}
trap cleanup EXIT

# losetup -P asks the kernel to register partition block devices, but this
# container's /dev is a plain tmpfs (no devtmpfs, no udev), so nothing
# creates the /dev nodes on its own; make them by hand from sysfs.
for part in "${base}p1" "${base}p2"; do
  if [[ ! -e "/dev/$part" ]]; then
    devno="$(cat "/sys/block/${base}/${part}/dev")"
    mknod "/dev/$part" b "${devno%%:*}" "${devno##*:}"
  fi
done

mkdir -p /mnt/root /mnt/esp
mount -o ro "/dev/${base}p2" /mnt/root
mount -o ro "/dev/${base}p1" /mnt/esp

note "checking /usr/local/bin/omnifs-fuse"
bin=/mnt/root/usr/local/bin/omnifs-fuse
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

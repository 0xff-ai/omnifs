#!/bin/sh
# Mounts the per-launch attach-parameter seed ISO (built fresh by the host on
# every libkrun launch, see scripts/guest-image/make-seed-iso.sh) by volume
# label, read-only, at /mnt/seed. No cloud-init and no NoCloud data source:
# this is a plain ISO9660 volume the host attaches as a virtio-blk device,
# identified purely by its label so the guest never has to guess which
# device node the runtime assigned it.
#
# Runs as omnifs-seed-mount.service, ordered before omnifs-frontend.service,
# which sources the config file this mounts via EnvironmentFile=. Failing
# here (seed never attached, or attached but not yet enumerated by udev) must
# be loud in the journal, not a silent hang: this script bounds its wait and
# exits non-zero with a clear message, and the seed-mount unit's failure
# blocks omnifs-frontend.service from starting.
set -eu

label=OMNIFS-SEED
device="/dev/disk/by-label/${label}"
mount_point=/mnt/seed
max_tries=20
sleep_seconds=0.5

tries=0
while [ ! -e "$device" ] && [ "$tries" -lt "$max_tries" ]; do
  tries=$((tries + 1))
  sleep "$sleep_seconds"
done

if [ ! -e "$device" ]; then
  echo "omnifs-seed-mount: no volume labeled ${label} found after waiting; the guest has no attach parameters and cannot start the frontend runner" >&2
  exit 1
fi

mkdir -p "$mount_point"
mount -o ro "$device" "$mount_point"
echo "omnifs-seed-mount: mounted ${device} at ${mount_point}"

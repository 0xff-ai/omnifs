#!/bin/sh
# Install the seed's ssh public key (OMNIFS_SSH_PUBKEY in
# /mnt/seed/omnifs-seed.conf) into root's authorized_keys and start the vsock
# ssh path: dropbear on guest loopback plus the vsock-to-loopback proxy
# socket. Runs as omnifs-ssh-setup.service, ordered after
# omnifs-seed-mount.service, which mounts the seed this reads.
#
# No key in the seed means ssh access is disabled for this launch: neither
# dropbear nor the socket is started. That must be visible in the journal
# (this script's own stderr, captured by the unit), not a silent no-op, since
# a guest with no ssh path is otherwise indistinguishable from one that is
# still booting.
set -eu

seed_conf=/mnt/seed/omnifs-seed.conf
pubkey=""
if [ -r "$seed_conf" ]; then
  pubkey=$(sed -n 's/^OMNIFS_SSH_PUBKEY=//p' "$seed_conf")
fi

if [ -z "$pubkey" ]; then
  echo "omnifs-ssh-setup: no OMNIFS_SSH_PUBKEY in the seed; ssh access is disabled for this launch" >&2
  exit 0
fi

mkdir -p /root/.ssh
chmod 700 /root/.ssh
printf '%s\n' "$pubkey" >/root/.ssh/authorized_keys
chmod 600 /root/.ssh/authorized_keys

echo "omnifs-ssh-setup: installed the seed's ssh public key; starting dropbear and the vsock ssh socket"
# Start dropbear eagerly (not just as the proxy's dependency) so it is already
# listening before the first host connection is proxied to it:
# systemd-socket-proxyd dials once per connection and does not retry a refused
# loopback dial.
systemctl start omnifs-dropbear.service
systemctl start omnifs-ssh.socket

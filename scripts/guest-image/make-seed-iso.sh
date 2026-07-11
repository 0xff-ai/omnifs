#!/usr/bin/env bash
# Build a per-launch attach-parameter seed ISO for the krunkit guest image.
#
# Plain config-drive, not cloud-init and not NoCloud: an ISO9660+Joliet volume
# labeled OMNIFS-SEED containing one KEY=VALUE file the guest's
# omnifs-frontend.service reads via systemd's EnvironmentFile=. macOS builds
# it with the native hdiutil (no mkisofs/xorriso dependency); a krunkit launch
# regenerates it fresh every time, since the attach token is per-instance.
#
# Usage: make-seed-iso.sh --out PATH --attach-addr HOST:PORT --attach-token
#   TOKEN [--ready-vsock-port PORT] [--ssh-pubkey KEY]
set -euo pipefail

seed_label=OMNIFS-SEED

out=""
attach_addr=""
attach_token=""
ready_vsock_port="0"
ssh_pubkey=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --out)
      out="$2"
      shift 2
      ;;
    --attach-addr)
      attach_addr="$2"
      shift 2
      ;;
    --attach-token)
      attach_token="$2"
      shift 2
      ;;
    --ready-vsock-port)
      ready_vsock_port="$2"
      shift 2
      ;;
    --ssh-pubkey)
      ssh_pubkey="$2"
      shift 2
      ;;
    *)
      echo "make-seed-iso.sh: unknown argument: $1" >&2
      exit 2
      ;;
  esac
done

: "${out:?--out PATH is required}"
: "${attach_addr:?--attach-addr HOST:PORT is required}"
: "${attach_token:?--attach-token TOKEN is required}"

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "make-seed-iso.sh: hdiutil is macOS-only; this script has no other backend" >&2
  exit 1
fi

staging="$(mktemp -d)"
trap 'rm -rf "$staging"' EXIT

# EnvironmentFile= format (systemd.exec(5)): KEY=VALUE lines, no quoting.
# OMNIFS_ATTACH_ADDR / OMNIFS_ATTACH_TOKEN are the same env vars the Docker
# frontend launcher injects (crates/omnifs-api/src/lib.rs), addressed as
# `vsock:<port>` instead of `host:port` for the krunkit transport
# (docs/contracts/40-frontends.md). OMNIFS_READY_VSOCK_PORT is the port the
# runner dials on host CID to signal the FUSE mount is serving
# (crates/omnifs-vfs-wire/src/beacon.rs). OMNIFS_SSH_PUBKEY, when given, is
# installed into root's authorized_keys before the vsock ssh socket starts
# (scripts/guest-image/mkosi/mkosi.extra/usr/local/lib/omnifs/setup-ssh.sh);
# omitting it (the default here, since this script's only caller today is
# smoke.sh) leaves ssh disabled for that launch, which is the intended "no
# key" smoke path.
cat >"$staging/omnifs-seed.conf" <<EOF
OMNIFS_ATTACH_ADDR=${attach_addr}
OMNIFS_ATTACH_TOKEN=${attach_token}
OMNIFS_READY_VSOCK_PORT=${ready_vsock_port}
EOF
if [[ -n "$ssh_pubkey" ]]; then
  echo "OMNIFS_SSH_PUBKEY=${ssh_pubkey}" >>"$staging/omnifs-seed.conf"
fi

rm -f "$out"
hdiutil makehybrid \
  -iso -joliet \
  -iso-volume-name "$seed_label" \
  -joliet-volume-name "$seed_label" \
  -o "$out" \
  "$staging" >/dev/null

echo "wrote seed ISO: $out"

# Containerized mkosi builder for the libkrun guest image (see
# scripts/guest-image/build.sh). The host is macOS; mkosi itself needs Linux
# (loop devices, systemd-repart, dpkg) to assemble a Debian raw disk image,
# so it runs here instead.
#
# Debian trixie's `mkosi` package Depends on everything mkosi needs to build
# a Debian image (systemd-repart, systemd-boot-efi, systemd-ukify, dosfstools,
# mtools, btrfs-progs, e2fsprogs, cryptsetup-bin, ...); --no-install-recommends
# skips its optional qemu/ovmf/virtiofsd Recommends, which this build never
# exercises (booting happens on the host via the krunkit executable, not inside this
# container).
FROM debian:trixie-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends mkosi \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /work
ENTRYPOINT ["mkosi"]

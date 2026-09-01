#!/usr/bin/env bash
# Cross-build the mosd, apid and mos-mqttd release binaries for aarch64.
# Kept as its own entry point because rootfs/build.sh and the board
# documentation both name it; the work is in build-target.sh, which the amd64
# QEMU image uses with a different pair of arguments.
set -euo pipefail
exec bash "$(dirname "$0")/build-target.sh" aarch64-unknown-linux-gnu aarch64

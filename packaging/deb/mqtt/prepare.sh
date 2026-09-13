#!/usr/bin/env bash
# The mqtt producer's PREPARE hook: cross-compile mica-mqttd and mica-mqtt-broker's binaries, assert that
# nothing this producer does not own was compiled with them, and leave the
# result in MICA_DEB_STAGE for build-env/deb/build.sh to pack.
#
# THE CRATE LIST LIVES HERE, per producer, because it is the one thing about
# this producer that no key in producer.env could describe: it is an input to a
# cargo build, not to a docker build. That is what a PREPARE hook is for.
set -euo pipefail

exec bash "${MICA_DEB_REPO_ROOT}/scripts/build/build-deb.sh" \
    --producer "${MICA_DEB_PRODUCER}" \
    --crates "mica-mqttd mica-mqtt-broker" \
    --arch "${MICA_DEB_ARCH}" \
    --stage "${MICA_DEB_STAGE}"

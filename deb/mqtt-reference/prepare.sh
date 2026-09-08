#!/usr/bin/env bash
set -euo pipefail
exec bash "${MOS_DEB_REPO_ROOT}/pkgs/mosd/hack/build-deb.sh" \
    --producer "$MOS_DEB_PRODUCER" --crates mos-mqtt-reference \
    --arch "$MOS_DEB_ARCH" --stage "$MOS_DEB_STAGE"

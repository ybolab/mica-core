#!/usr/bin/env bash
set -euo pipefail
exec bash "${MICA_DEB_REPO_ROOT}/scripts/build/build-deb.sh" \
    --producer "${MICA_DEB_PRODUCER}" --bins mica-deploy \
    --arch "${MICA_DEB_ARCH}" --stage "${MICA_DEB_STAGE}"

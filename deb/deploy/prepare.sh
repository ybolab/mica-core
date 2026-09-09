#!/usr/bin/env bash
set -euo pipefail
exec bash "${MOS_DEB_REPO_ROOT}/pkgs/mos-deploy/hack/build-deb.sh" \
    --producer "${MOS_DEB_PRODUCER}" --bins mos-deploy \
    --arch "${MOS_DEB_ARCH}" --stage "${MOS_DEB_STAGE}"

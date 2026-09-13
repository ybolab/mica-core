#!/usr/bin/env bash
# The lifecycle producer's PREPARE hook: compile the two static lifecycle
# executables into MOS_DEB_STAGE for build-env/deb/build.sh to hand the
# packaging build as its `bin` context. Everything about the compile lives in
# hack/build-deb.sh; this only says which binaries this producer owns.
set -euo pipefail
exec bash "${MOS_DEB_REPO_ROOT}/hack/build-deb.sh" \
    --producer "${MOS_DEB_PRODUCER}" --bins "mica-init mica-shutdown" \
    --arch "${MOS_DEB_ARCH}" --stage "${MOS_DEB_STAGE}"

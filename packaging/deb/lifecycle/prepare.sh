#!/usr/bin/env bash
# The lifecycle producer's PREPARE hook: compile the static lifecycle
# executable into MICA_DEB_STAGE for build-env/deb/build.sh to hand the
# packaging build as its `bin` context. Everything about the compile lives in
# scripts/build/build-deb.sh; this only says which binaries this producer owns.
set -euo pipefail
exec bash "${MICA_DEB_REPO_ROOT}/scripts/build/build-deb.sh" \
    --producer "${MICA_DEB_PRODUCER}" --bins mica-runkit \
    --arch "${MICA_DEB_ARCH}" --stage "${MICA_DEB_STAGE}"

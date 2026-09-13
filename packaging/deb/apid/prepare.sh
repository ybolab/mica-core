#!/usr/bin/env bash
# The apid producer's PREPARE hook: cross-compile the mica-apid executable
# (with the built-in UI embedded), assert that nothing this producer does not
# own was compiled with it, and leave the result in MICA_DEB_STAGE for
# build-env/deb/build.sh to pack.
set -euo pipefail

bash "${MICA_DEB_REPO_ROOT}/scripts/build/build-deb.sh" \
    --producer "${MICA_DEB_PRODUCER}" \
    --bins mica-apid \
    --arch "${MICA_DEB_ARCH}" \
    --stage "${MICA_DEB_STAGE}"
# The committed OpenAPI document rides in the mica-apid payload as
# /usr/share/mica-apid/openapi.json: the assembly's API harness pins its
# phase literals against it and reads it out of the archive it installs
# rather than out of a checkout of this repository. scripts/build/check.sh
# has already asserted it is what mica-apid --openapi prints.
cp "${MICA_DEB_REPO_ROOT}/crates/mica-apid/openapi.json" "${MICA_DEB_STAGE}/openapi.json"

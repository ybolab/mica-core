#!/usr/bin/env bash
# The micad producer's PREPARE hook: cross-compile the micad binary -- micad, and
# apid reached through its `apid` link -- assert that nothing this producer does
# not own was compiled with it, and leave the
# result in MICA_DEB_STAGE for build-env/deb/build.sh to pack.
#
# THE CRATE LIST LIVES HERE, per producer, because it is the one thing about
# this producer that no key in producer.env could describe: it is an input to a
# cargo build, not to a docker build. That is what a PREPARE hook is for.
set -euo pipefail

bash "${MICA_DEB_REPO_ROOT}/scripts/build/build-deb.sh" \
    --producer "${MICA_DEB_PRODUCER}" \
    --bins micad \
    --arch "${MICA_DEB_ARCH}" \
    --stage "${MICA_DEB_STAGE}"
# The committed OpenAPI document rides in the mica-apid payload as
# /usr/share/mica-apid/openapi.json: the assembly's API harness pins its
# phase literals against it (tests/apid-api/spec-pins.sh there) and reads
# it out of the archive it installs rather than out of a checkout of this
# repository. scripts/build/check.sh has already asserted it is what apid --openapi
# prints.
cp "${MICA_DEB_REPO_ROOT}/crates/mica-apid/openapi.json" "${MICA_DEB_STAGE}/openapi.json"

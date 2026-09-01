#!/usr/bin/env bash
# The apid API suite's build-time check: every phase literal that is ALSO
# stated in pkgs/mosd/apid/openapi.json, asserted to agree with it.
#
#   bash pkgs/mosd/tests/apid-api/spec-pins.sh
#   make os-apid-api-spec-pins
#
# NO NETWORK, NO QEMU, NO IMAGE. This is the half of `pkgs/mosd/tests/apid-api` that can
# be run on a checkout: it reads the phase files' own bytes and the committed
# OpenAPI document and compares them. The other half -- run.sh -- needs a built
# image and a nine-minute boot, which is exactly why the pins it carries could
# go stale unnoticed. src/spec-pins.ts's header states what is and is not in
# scope; the full black-box suite covers the remaining runtime contracts.
#
# Same two routes as verify/run.sh, for the same reason: a developer with a
# bun runs on it, and a host without one runs the bun this tree pins by digest
# as IMAGE_BUN_1. The announce line says which answered, because "it passed" is
# not a result until you know what produced it.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../../../.." && pwd)"

BUN=""
ROUTE=host
WHY=""
if [ "${MOS_APID_CONTAINER:-0}" = 1 ]; then
    ROUTE=container
    WHY="MOS_APID_CONTAINER=1"
elif command -v bun >/dev/null 2>&1; then
    BUN="$(command -v bun)"
elif [ -x "${HOME:-/root}/.bun/bin/bun" ]; then
    BUN="${HOME:-/root}/.bun/bin/bun"
else
    ROUTE=container
    WHY="no bun on this host"
fi

if [ "${ROUTE}" = container ]; then
    command -v docker >/dev/null 2>&1 || {
        echo "error: no bun on this host, and no docker to run the pinned one in." >&2
        echo "       This check needs one of the two. The bun this tree runs is recorded as" >&2
        echo "       IMAGE_BUN_1 in build-env/images.env and needs a container runtime to be it." >&2
        exit 1
    }
    # One resolver, the tree's own: from.sh validates that the key exists and is
    # a digest rather than a tag, and says so naming the key and the file.
    BUN_IMAGE="${MOS_APID_BUN_IMAGE:-$(bash "${REPO_ROOT}/build-env/from.sh" --ref IMAGE_BUN_1)}"
    if ! docker image inspect "${BUN_IMAGE}" >/dev/null 2>&1; then
        echo "apid-api spec-pins: ${BUN_IMAGE} is not in the local image store; pulling it"
        docker pull -q "${BUN_IMAGE}" >/dev/null 2>&1 || {
            echo "error: IMAGE_BUN_1=${BUN_IMAGE} could not be obtained. A run that continued" >&2
            echo "       past this would be a run by an unknown bun." >&2
            exit 1
        }
    fi
    echo "apid-api spec-pins: in ${BUN_IMAGE} (${WHY})"
    # No `bun install`: src/spec-pins.ts imports nothing from node_modules, so
    # the check runs on a checkout with no dependencies fetched and no network.
    exec docker run --rm -v "${REPO_ROOT}:/w" -w /w/pkgs/mosd/tests/apid-api \
        "${BUN_IMAGE}" bun run src/spec-pins.ts
fi

echo "apid-api spec-pins: $("${BUN}" --version 2>/dev/null || echo '?') at ${BUN}"
cd "${SCRIPT_DIR}"
exec "${BUN}" run src/spec-pins.ts

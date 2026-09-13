#!/usr/bin/env bash
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${HERE}/../../.." && pwd)"
BUILD_ROOT="${REPO_ROOT}/_out/apid-ui"
OUTPUT="${BUILD_ROOT}/dist"
RUN_CHECKS=0

case "${1-}" in
"") ;;
--check) RUN_CHECKS=1 ;;
*)
    echo "usage: bash crates/mica-apid/ui/build.sh [--check]" >&2
    exit 1
    ;;
esac
[ "$#" -le 1 ] || {
    echo "usage: bash crates/mica-apid/ui/build.sh [--check]" >&2
    exit 1
}

command -v docker >/dev/null 2>&1 || {
    echo "error: docker is required to build the apid UI" >&2
    exit 1
}

for path in \
    "${REPO_ROOT}/_out" \
    "${BUILD_ROOT}" \
    "${BUILD_ROOT}/home" \
    "${BUILD_ROOT}/work" \
    "${OUTPUT}"
do
    [ ! -L "${path}" ] || {
        echo "error: refusing symlinked UI build path ${path}" >&2
        exit 1
    }
    [ ! -e "${path}" ] || [ -d "${path}" ] || {
        echo "error: UI build path ${path} exists but is not a directory" >&2
        exit 1
    }
done

mkdir -p "${BUILD_ROOT}/home" "${BUILD_ROOT}/work" "${OUTPUT}"
image="$(bash "${REPO_ROOT}/build-env/from.sh" --ref IMAGE_MICA_BUILD_BASE)"

if [ "${RUN_CHECKS}" = 1 ]; then
    echo "apid UI checks: ${image} -> ${OUTPUT}"
else
    echo "apid UI build: ${image} -> ${OUTPUT}"
fi

# mica-build-side: container-block -- the UI is built by the bun inside mica-build-base, pinned as IMAGE_MICA_BUILD_BASE,
# with the source mounted read-only; a host bun produces different chunk hashes, so
# there is deliberately no host route here
docker run --rm \
    --label ai-agent=true \
    --user "$(id -u):$(id -g)" \
    -v "${HERE}:/source:ro" \
    -v "${BUILD_ROOT}:/build" \
    -e HOME=/build/home \
    -e "MICA_APID_UI_RUN_CHECKS=${RUN_CHECKS}" \
    --entrypoint /bin/bash \
    "${image}" -c '
        set -euo pipefail

        find /build/work -mindepth 1 -delete
        find /build/dist -mindepth 1 -delete
        tar -C /source \
            --exclude=./coverage \
            --exclude=./dist \
            --exclude=./node_modules \
            --exclude=./playwright-report \
            --exclude=./test-results \
            -cf - . | tar -C /build/work -xf -

        cd /build/work
        bun install --frozen-lockfile
        if [ "${MICA_APID_UI_RUN_CHECKS}" = 1 ]; then
            # The UI library policy first: it is the cheapest check and the one
            # that catches a hand-written primitive or a re-styled registry
            # component before the type checker spends a minute agreeing the
            # code is valid.
            bash verify-ui-policy.sh
            bun run lint
            bun run typecheck
            bun run test
        fi
        bun run build -- --outDir /build/dist --emptyOutDir

        [ -s /build/dist/index.html ] || {
            echo "error: the apid UI build did not produce a non-empty index.html" >&2
            exit 1
        }
        find /build/work -mindepth 1 -delete
    '
# mica-build-side: host

if [ "${RUN_CHECKS}" = 1 ]; then
    echo "APID UI CHECKS PASSED"
else
    echo "APID UI BUILD PASSED"
fi

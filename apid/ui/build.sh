#!/usr/bin/env bash
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${HERE}/../../../.." && pwd)"
OUTPUT="${HERE}/dist"
USE_CONTAINER=0

while [ "$#" -gt 0 ]; do
    case "$1" in
    --container)
        USE_CONTAINER=1
        shift
        ;;
    --out-dir)
        OUTPUT="${2-}"
        [ -n "${OUTPUT}" ] || {
            echo "error: --out-dir takes a directory" >&2
            exit 1
        }
        shift 2
        ;;
    *)
        echo "usage: bash pkgs/mosd/apid/ui/build.sh [--container] [--out-dir DIR]" >&2
        exit 1
        ;;
    esac
done

case "${OUTPUT}" in
/*) ;;
*) OUTPUT="$(pwd)/${OUTPUT}" ;;
esac

[ ! -L "${OUTPUT}" ] || {
    echo "error: refusing the symlinked UI output directory ${OUTPUT}" >&2
    exit 1
}
OUTPUT="$(realpath -m -- "${OUTPUT}")"
[ ! -e "${OUTPUT}" ] || [ -d "${OUTPUT}" ] || {
    echo "error: the UI output path ${OUTPUT} exists but is not a directory" >&2
    exit 1
}

[ "${OUTPUT}" != "/" ] || {
    echo "error: refusing to use / as the UI output directory" >&2
    exit 1
}
case "${HERE}/" in
"${OUTPUT%/}/"*)
    echo "error: refusing UI output directory ${OUTPUT} because it contains the source tree" >&2
    exit 1
    ;;
esac
case "${OUTPUT}" in
"${HERE}/dist" | "${HERE}/dist/"* | "${REPO_ROOT}/_out/"*) ;;
*)
    echo "error: UI output must be ui/dist or a directory below ${REPO_ROOT}/_out" >&2
    exit 1
    ;;
esac

if [ "${USE_CONTAINER}" = 1 ] || ! command -v bun >/dev/null 2>&1; then
    command -v docker >/dev/null 2>&1 || {
        echo "error: building the apid UI needs bun or docker" >&2
        exit 1
    }
    case "${OUTPUT}" in
    "${REPO_ROOT}"/*) container_output="/workspace/${OUTPUT#"${REPO_ROOT}"/}" ;;
    *)
        echo "error: --out-dir must be below ${REPO_ROOT} when Bun is supplied by Docker" >&2
        exit 1
        ;;
    esac
    image="$(bash "${REPO_ROOT}/build-env/from.sh" --ref IMAGE_BUN_1)"
    if [ "${USE_CONTAINER}" = 1 ]; then
        echo "apid UI build: ${image} (pinned container requested)"
    else
        echo "apid UI build: ${image} (no bun on this host)"
    fi
    exec docker run --rm \
        --label ai-agent=true \
        -v "${REPO_ROOT}:/workspace" \
        -w /workspace/pkgs/mosd/apid/ui \
        "${image}" bash ./build.sh --out-dir "${container_output}"
fi

cd "${HERE}"
echo "apid UI build: bun $(bun --version) -> ${OUTPUT}"
bun install --frozen-lockfile
bun run build -- --outDir "${OUTPUT}" --emptyOutDir

[ -s "${OUTPUT}/index.html" ] || {
    echo "error: the apid UI build did not produce a non-empty index.html in ${OUTPUT}" >&2
    exit 1
}

echo "APID UI BUILD PASSED"

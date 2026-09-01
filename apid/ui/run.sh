#!/usr/bin/env bash
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${HERE}/../../../.." && pwd)"

if ! command -v bun >/dev/null 2>&1; then
    command -v docker >/dev/null 2>&1 || {
        echo "error: the apid UI check needs bun or docker" >&2
        exit 1
    }
    image="$(bash "${REPO_ROOT}/build-env/from.sh" --ref IMAGE_BUN_1)"
    echo "apid UI: ${image} (no bun on this host)"
    exec docker run --rm \
        -v "${REPO_ROOT}:/workspace" \
        -w /workspace/pkgs/mosd/apid/ui \
        "${image}" bash ./run.sh
fi

cd "${HERE}"
echo "apid UI: bun $(bun --version)"
bun install --frozen-lockfile
bun run lint
bun run typecheck
bun run test

build_root="$(mktemp -d)"
trap 'rm -rf "${build_root}"' EXIT
bun x vite build --outDir "${build_root}/dist"

diff -ruN dist "${build_root}/dist" || {
    echo "error: apid/ui/dist differs from a clean production build" >&2
    echo "       run 'bun run build' in pkgs/mosd/apid/ui and commit the result" >&2
    exit 1
}

echo "APID UI CHECKS PASSED"

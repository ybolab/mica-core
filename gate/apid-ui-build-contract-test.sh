#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REL_DIST="apid/ui/dist"
DIST_PROBE="${REL_DIST}/index.html"
BUILD_SCRIPT="${ROOT}/apid/ui/build.sh"
CHECK_SCRIPT="${ROOT}/apid/ui/run.sh"
OUTPUT_REL="_out/apid-ui/dist"

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

tracked="$(git -C "${ROOT}" ls-files -- "${REL_DIST}")"
[ -z "${tracked}" ] || fail "${REL_DIST}/ is generated output but Git still tracks files below it"

git -C "${ROOT}" check-ignore --no-index -q "${DIST_PROBE}" ||
    fail "${REL_DIST}/ is generated output but is not ignored"

[ -f "${BUILD_SCRIPT}" ] ||
    fail "the built-in UI has no production build entry"

grep -q 'IMAGE_MICA_BUILD_BASE' "${BUILD_SCRIPT}" ||
    fail "the production build does not select the pinned mica-build-base image, whose bun builds the UI"
if grep -q 'IMAGE_BUN_1' "${BUILD_SCRIPT}"; then
    fail "the production build still selects IMAGE_BUN_1, a second bun"
fi
grep -q -- '-v "${HERE}:/source:ro"' "${BUILD_SCRIPT}" ||
    fail "the production build does not mount UI source read-only"
grep -q -- '-v "${BUILD_ROOT}:/build"' "${BUILD_SCRIPT}" ||
    fail "the production build does not isolate writes below _out/apid-ui"
grep -q 'OUTPUT="${BUILD_ROOT}/dist"' "${BUILD_SCRIPT}" ||
    fail "the production build output is not fixed at ${OUTPUT_REL}"
grep -q '\[ ! -L "${path}" \]' "${BUILD_SCRIPT}" ||
    fail "the production build does not refuse symlinked build-root components"
grep -q -- '--user "$(id -u):$(id -g)"' "${BUILD_SCRIPT}" ||
    fail "the production build does not preserve caller ownership on generated files"
if grep -q 'command -v bun' "${BUILD_SCRIPT}"; then
    fail "the production build still selects a host Bun installation"
fi

grep -q 'build.sh" --check' "${CHECK_SCRIPT}" ||
    fail "the frontend quality gate does not reuse the container-only producer"
if grep -Eq 'bun (install|run)|command -v bun' "${CHECK_SCRIPT}"; then
    fail "the frontend quality gate still owns a second Bun execution path"
fi

grep -q 'MICA_APID_UI_DIST_DIR' "${ROOT}/apid/build.rs" ||
    fail "apid/build.rs does not require the generated UI directory"

for entry in \
    "hack/check.sh" \
    "hack/build-target.sh" \
    "hack/build-deb.sh"
do
    grep -q 'apid/ui/build.sh' "${ROOT}/${entry}" ||
        fail "${entry} does not build the UI before Cargo"
    grep -q 'MICA_APID_UI_DIST_DIR' "${ROOT}/${entry}" ||
        fail "${entry} does not pass the generated UI directory to Cargo"
    if grep -q 'apid/ui/dist' "${ROOT}/${entry}"; then
        fail "${entry} still consumes generated assets from the UI source tree"
    fi
done

for entry in \
    "hack/build-target.sh" \
    "hack/build-deb.sh"
do
    grep -q -- '-v "${REPO_ROOT}:/src:ro"' "${ROOT}/${entry}" ||
        fail "${entry} does not mount repository source read-only"
    grep -q -- '-v "${APID_UI_DIST}:/build/apid-ui:ro"' "${ROOT}/${entry}" ||
        fail "${entry} does not mount generated UI assets read-only"
    grep -q 'MICA_APID_UI_DIST_DIR=/build/apid-ui' "${ROOT}/${entry}" ||
        fail "${entry} does not name the isolated UI mount for Cargo"
    grep -q 'CARGO_TARGET_DIR=/target' "${ROOT}/${entry}" ||
        fail "${entry} does not move Cargo writes out of the source tree"
done

if grep -q 'apid/ui/dist' "${ROOT}/.github/workflows/check.yml"; then
    fail "CI still consumes generated UI assets from the source tree"
fi

echo "APID UI BUILD CONTRACT PASSED"

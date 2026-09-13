#!/usr/bin/env bash
# The Rust gate: scripts/build/check.sh, UNMODIFIED, inside localhost/mica-build-rust-check
# -- the VERSION agreement, `cargo fmt --all --check`, clippy at `-D warnings`,
# nextest, doctests, `cargo deny check licenses bans advisories` and the
# OpenAPI document against `apid --openapi`. The built-in UI is built first
# in the pinned Bun image, because apid embeds it and the gate compiles apid.
# The repository is mounted read-only at the fixed path /src, so rustc's
# recorded paths do not depend on where the checkout lives and the gate
# cannot fix what it found; the image records what it is and the log reads
# that back, so the result names the toolchain that produced it.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${HERE}/../.." && pwd)"
for p in "${REPO_ROOT}/Cargo.toml" "${REPO_ROOT}/build-env/from.sh"; do
    [ -e "${p}" ] || {
        echo "error: ${p} does not exist. scripts/gate/rust-gate.sh derives the repository as two levels above itself; build-env/ is the mica-build-env source pin (make deps)" >&2
        exit 1
    }
done
[ -x "${REPO_ROOT}/scripts/build/check.sh" ] || { echo "error: ${REPO_ROOT}/scripts/build/check.sh is missing or not executable; this gate runs that script and defines no gate of its own" >&2; exit 1; }
command -v docker >/dev/null 2>&1 || { echo "error: docker is required and not on PATH; the gate runs in the pinned rust-check image" >&2; exit 1; }

IMAGE_ARCH=amd64
mapfile -t FROM < <(bash "${REPO_ROOT}/build-env/from.sh" --arch="${IMAGE_ARCH}" MICA_BUILD_RUST_CHECK=LOCAL_MICA_BUILD_RUST_CHECK)
[ "${#FROM[@]}" -eq 2 ] || { echo "error: build-env/from.sh did not yield localhost/mica-build-rust-check:${IMAGE_ARCH} (see its message above); it is built by \`make build-env\`" >&2; exit 1; }
IMAGE="${FROM[1]#MICA_BUILD_RUST_CHECK=}"

APID_UI_DIST="${REPO_ROOT}/_out/apid-ui/dist"
bash "${REPO_ROOT}/crates/mica-apid/ui/build.sh"

CARGO_CACHE="${REPO_ROOT}/_out/cargo"
TARGET_DIR="${REPO_ROOT}/_out/rust-gate"
mkdir -p "${CARGO_CACHE}/registry" "${CARGO_CACHE}/git" "${TARGET_DIR}"

echo "=== rust gate: micad in ${IMAGE} ==="
docker run --rm \
    --label ai-agent=true \
    --platform "linux/${IMAGE_ARCH}" \
    -v "${REPO_ROOT}:/src:ro" \
    -v "${TARGET_DIR}:/target" \
    -v "${CARGO_CACHE}/registry:/usr/local/cargo/registry" \
    -v "${CARGO_CACHE}/git:/usr/local/cargo/git" \
    -v "${APID_UI_DIST}:/build/apid-ui:ro" \
    -w /src \
    -e "CARGO_TARGET_DIR=/target" \
    -e "MICA_APID_UI_DIST_DIR=/build/apid-ui" \
    --entrypoint /bin/bash \
    "${IMAGE}" -c '
        set -euo pipefail
        [ -f /etc/mica-build/rust-check.env ] || {
            echo "error: this image carries no /etc/mica-build/rust-check.env, so what ran this gate cannot be read back out of it" >&2
            exit 1
        }
        . /etc/mica-build/rust-check.env
        echo "gate: micad with rustc ${MICA_BUILD_RUSTC}, clippy ${MICA_BUILD_CLIPPY}, rustfmt ${MICA_BUILD_RUSTFMT}, nextest ${MICA_BUILD_NEXTEST}, deny ${MICA_BUILD_DENY} from ${MICA_BUILD_IMAGE}"
        bash scripts/build/check.sh
    '
echo "RUST GATE PASSED (micad)"

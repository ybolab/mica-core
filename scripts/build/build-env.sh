#!/usr/bin/env bash
# The mica-build-env release this repository builds on, at its pin.
#
#   bash scripts/build/build-env.sh fetch     download and verify into build-env/
#   bash scripts/build/build-env.sh check     verify build-env/ against the pin, no network
#
# The pin is deps/build-env.json: the release version and the sha256 of its
# SHA256SUMS (RULES.md 1). fetch downloads the three release assets from
# https://github.com/ybolab/mica-build-env/releases/download/<version>/ and
# refuses them unless SHA256SUMS hashes to the pin and `sha256sum -c
# SHA256SUMS` passes; only then does build-env/ hold them. build-env/images.env
# is the release asset every image reference here resolves through
# (scripts/build/from.sh); the archive is kept beside it because SHA256SUMS
# covers it, and nothing runs out of it. build-env/ is gitignored and never
# edited: a change is a new pin.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${HERE}/../.." && pwd)"
PIN="${REPO_ROOT}/deps/build-env.json"
DEST="${REPO_ROOT}/build-env"
URL_BASE="${MICA_BUILD_ENV_RELEASES:-https://github.com/ybolab/mica-build-env/releases/download}"

die() { echo "build-env.sh: error: $*" >&2; exit 1; }
for t in curl jq sha256sum; do
    command -v "${t}" >/dev/null 2>&1 || die "${t} is required and not on PATH"
done
[ -f "${PIN}" ] || die "${PIN#"${REPO_ROOT}"/} does not exist; it is the release pin"
jq -e 'type == "object" and (keys == ["sha256sums","version"])
    and (.version | test("^v[0-9]+\\.[0-9]+\\.[0-9]+$"))
    and (.sha256sums | test("^[0-9a-f]{64}$"))' "${PIN}" >/dev/null ||
    die "${PIN#"${REPO_ROOT}"/} is not a release pin: an object with exactly version (v<X.Y.Z>) and sha256sums (64 lowercase hex)"
VERSION="$(jq -r .version "${PIN}")"
SUMS_SHA="$(jq -r .sha256sums "${PIN}")"
ASSETS=("mica-build-env-${VERSION}.tar.gz" images.env)

# SHA256SUMS at the pin, and exactly the release's two assets in it, each present and matching.
verify() {
    local dir="$1" got names
    [ -f "${dir}/SHA256SUMS" ] || { echo "no SHA256SUMS"; return 1; }
    got="$(sha256sum "${dir}/SHA256SUMS" | cut -d' ' -f1)"
    [ "${got}" = "${SUMS_SHA}" ] || { echo "SHA256SUMS hashes to ${got}, and the pin records ${SUMS_SHA}"; return 1; }
    names="$(sed 's/^[0-9a-f]\{64\}  //' "${dir}/SHA256SUMS" | LC_ALL=C sort | tr '\n' ' ')"
    [ "${names}" = "$(printf '%s\n' "${ASSETS[@]}" | LC_ALL=C sort | tr '\n' ' ')" ] ||
        { echo "SHA256SUMS lists '${names% }', not the ${VERSION} assets ${ASSETS[*]}"; return 1; }
    (cd "${dir}" && sha256sum --quiet --strict -c SHA256SUMS) >&2 || { echo "sha256sum -c SHA256SUMS failed"; return 1; }
}

case "${1:-}" in
check)
    [ "$#" -eq 1 ] || die "usage: bash scripts/build/build-env.sh fetch | check"
    why="$(verify "${DEST}")" || die "build-env/ is not mica-build-env ${VERSION} at the pin: ${why}. Run: make deps"
    echo "build-env.sh: build-env/ is mica-build-env ${VERSION} (SHA256SUMS ${SUMS_SHA:0:12}, verified)"
    ;;
fetch)
    [ "$#" -eq 1 ] || die "usage: bash scripts/build/build-env.sh fetch | check"
    if verify "${DEST}" >/dev/null 2>&1; then
        echo "build-env.sh: build-env/ is mica-build-env ${VERSION} already"
        exit 0
    fi
    mkdir -p "${REPO_ROOT}/tmp"
    work="$(mktemp -d "${REPO_ROOT}/tmp/build-env.XXXXXX")"
    trap 'rm -rf "${work}"' EXIT
    for f in SHA256SUMS "${ASSETS[@]}"; do
        curl -fsSL --retry 3 --max-time 300 -o "${work}/${f}" "${URL_BASE}/${VERSION}/${f}" ||
            die "downloading ${URL_BASE}/${VERSION}/${f} failed (see curl's message above)"
    done
    why="$(verify "${work}")" || die "the ${VERSION} assets were refused and discarded: ${why}"
    rm -rf "${DEST}"
    mv "${work}" "${DEST}"
    trap - EXIT
    echo "build-env.sh: build-env/ is now mica-build-env ${VERSION} (SHA256SUMS ${SUMS_SHA:0:12}, verified)"
    ;;
*) die "usage: bash scripts/build/build-env.sh fetch | check" ;;
esac

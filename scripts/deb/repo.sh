#!/usr/bin/env bash
# Index one architecture's package pool: Packages, SHA256SUMS and a manifest.
#
#   bash scripts/deb/repo.sh --arch <amd64|arm64>
#
#   reads   _out/debs/<arch>/pool/*.deb
#   writes  _out/debs/<arch>/{Packages,SHA256SUMS,manifest.txt}
#
#   MICA_POOL_DIR overrides _out/debs (the parent of the per-architecture pools).
#
# Everything written is read out of the archives, inside the IMAGE_MICA_BUILD_BASE image.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${HERE}/../.." && pwd)"
FROM_SH="${REPO_ROOT}/scripts/build/from.sh"
for p in "${REPO_ROOT}/Makefile" "${FROM_SH}"; do
    [ -e "${p}" ] || {
        echo "error: ${p} does not exist. scripts/deb/repo.sh derives REPO_ROOT as two levels above itself; if this file moved, that arithmetic moved with it" >&2
        exit 1
    }
done

ARCH=""
while [ "$#" -gt 0 ]; do
    case "$1" in
    --arch)
        ARCH="${2-}"
        [ -n "${ARCH}" ] || { echo "error: --arch takes amd64 or arm64" >&2; exit 1; }
        shift 2
        ;;
    *)
        echo "usage: bash scripts/deb/repo.sh --arch <amd64|arm64>" >&2
        exit 1
        ;;
    esac
done
case "${ARCH}" in
amd64 | arm64) ;;
*)
    echo "error: --arch must be amd64 or arm64; it selects the pool under _out/debs/ that is indexed" >&2
    exit 1
    ;;
esac

command -v docker >/dev/null 2>&1 || {
    echo "error: docker is required and not on PATH. dpkg-scanpackages and dpkg-deb run inside the IMAGE_MICA_BUILD_BASE image rather than on the host, which is what keeps the host free of a Debian toolchain" >&2
    exit 1
}

DIST="${MICA_POOL_DIR:-${REPO_ROOT}/_out/debs}/${ARCH}"
POOL="${DIST}/pool"
[ -d "${POOL}" ] || {
    echo "error: ${POOL} does not exist, so there is nothing to index. A package producer writes it; this only reads it" >&2
    exit 1
}
# APT accepts an empty index silently, so an empty pool is refused.
mapfile -t DEBS < <(find "${POOL}" -maxdepth 1 -type f -name '*.deb' -printf '%f\n' | LC_ALL=C sort)
[ "${#DEBS[@]}" -gt 0 ] || {
    echo "error: ${POOL} holds no .deb, so this would write an index over nothing. APT takes an empty Packages file without complaint, which makes a build that installs none of its own packages look green" >&2
    exit 1
}

# The host architecture's image: indexing only parses archives, and a foreign
# image may not run here. --arch selects the pool.
case "$(uname -m)" in
x86_64) IMAGE_ARCH=amd64 ;;
aarch64 | arm64) IMAGE_ARCH=arm64 ;;
*)
    echo "error: $(uname -m) is not an architecture the IMAGE_MICA_BUILD_BASE index carries, so there is no container to run dpkg-scanpackages in" >&2
    exit 1
    ;;
esac
mapfile -t FROM_ARGS < <(bash "${FROM_SH}" --arch="${IMAGE_ARCH}" MICA_BUILD_DEB=IMAGE_MICA_BUILD_BASE)
[ "${#FROM_ARGS[@]}" -eq 2 ] || {
    echo "error: scripts/build/from.sh did not yield IMAGE_MICA_BUILD_BASE (see its message above); build-env/images.env is the pinned release asset (make deps)" >&2
    exit 1
}
IMAGE="${FROM_ARGS[1]#MICA_BUILD_DEB=}"

# Only _out/debs/<arch> is mounted.
# mica-build-side: container-block -- the index is written by the dpkg inside
# the IMAGE_MICA_BUILD_BASE image, which records its own versions in /etc/mica-build/base.env
# and is read back by the block below
docker run --rm \
    --label ai-agent=true \
    -v "${DIST}:/dist" \
    -w /dist \
    -e "MICA_DEB_ARCH=${ARCH}" \
    --entrypoint /bin/bash \
    "${IMAGE}" -c '
        set -euo pipefail
        [ -f /etc/mica-build/base.env ] || {
            echo "error: this image carries no /etc/mica-build/base.env, so what indexed this repository cannot be read back out of it" >&2
            exit 1
        }
        # Read by key, not sourced: base.env records values with shell metacharacters unquoted.
        record() { sed -n "s/^$1=//p" /etc/mica-build/base.env; }
        echo "repo.sh: indexing ${MICA_DEB_ARCH} with dpkg $(record MICA_BUILD_DPKG) from $(record MICA_BUILD_IMAGE)"

        # Sorted, not readdir order.
        mapfile -t debs < <(cd pool && find . -maxdepth 1 -type f -name "*.deb" -printf "%f\n" | LC_ALL=C sort)

        # A cross-packed archive is well formed and installs nowhere.
        for d in "${debs[@]}"; do
            a="$(dpkg-deb --field "pool/${d}" Architecture)"
            case "${a}" in
            "${MICA_DEB_ARCH}" | all) ;;
            *)
                echo "error: pool/${d} declares Architecture: ${a}, but it is in the ${MICA_DEB_ARCH} pool. It was packed in the wrong container, or filed under the wrong architecture" >&2
                exit 1
                ;;
            esac
        done

        # Pool-relative paths, for `deb [trusted=yes] file:/dist ./`.
        dpkg-scanpackages --multiversion pool >Packages.new
        [ -s Packages.new ] || {
            echo "error: dpkg-scanpackages produced an empty Packages over ${#debs[@]} archive(s)" >&2
            exit 1
        }
        mv Packages.new Packages

        (printf "pool/%s\n" "${debs[@]}" | xargs -r sha256sum) >SHA256SUMS

        {
            echo "# The local package pool for ${MICA_DEB_ARCH}, read out of the archives by scripts/deb/repo.sh."
            echo "# Regenerated whenever the pool changes; never edited by hand."
            printf "#package\tversion\tarchitecture\tinstalled-size\tsha256\tfile\tsource-repo\tsource-commit\n"
            for d in "${debs[@]}"; do
                repo="$(dpkg-deb --field "pool/${d}" Mica-Source-Repo)"
                commit="$(dpkg-deb --field "pool/${d}" Mica-Source-Commit)"
                [ -n "${repo}" ] && [ -n "${commit}" ] || {
                    echo "error: pool/${d} carries no Mica-Source-Repo/Mica-Source-Commit control fields. scripts/deb/pack.sh writes both into every archive; one without them was packed by something else" >&2
                    exit 1
                }
                printf "%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n" \
                    "$(dpkg-deb --field "pool/${d}" Package)" \
                    "$(dpkg-deb --field "pool/${d}" Version)" \
                    "$(dpkg-deb --field "pool/${d}" Architecture)" \
                    "$(dpkg-deb --field "pool/${d}" Installed-Size)" \
                    "$(sha256sum "pool/${d}" | cut -d" " -f1)" \
                    "pool/${d}" "${repo}" "${commit}"
            done
        } >manifest.txt

        echo "repo.sh: ${#debs[@]} package(s), $(grep -c "^Package: " Packages) stanza(s) in Packages"
    '
# mica-build-side: host

echo "repo.sh: ${DIST}"
sed 's/^/  /' "${DIST}/manifest.txt"

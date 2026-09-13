#!/usr/bin/env bash
# Publish the archives this repository built as its pool artifacts.
#
#   bash scripts/deb/publish.sh [--pool <dir>] [--arch <amd64|arm64>]
#
#   reads   <pool>/<arch>/pool/*.deb     (default pool: _out/debs, both arches)
#   writes  <registry>/<this repository>:pool.<arch>.build-<commit12>,
#           one artifact per architecture, one layer per archive
#           (application/vnd.mica.deb, titled with the archive's name)
#
# One artifact per source commit and architecture; an `all` archive is a
# layer of both. Refused: dirty versions, archives from another repository or
# another commit, and a dirty checkout. A tag that already exists must carry
# exactly these archives at these digests; every blob is read back and
# compared by sha256.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${HERE}/../.." && pwd)"
# shellcheck disable=SC1091
. "${HERE}/registry.sh"

POOL_ROOT="${REPO_ROOT}/_out/debs"
ARCHES=(amd64 arm64)
while [ "$#" -gt 0 ]; do
    case "$1" in
    --pool) POOL_ROOT="${2-}"; [ -n "${POOL_ROOT}" ] || { echo "error: --pool takes a directory" >&2; exit 1; }; shift 2 ;;
    --arch) [ -n "${2-}" ] || { echo "error: --arch takes amd64 or arm64" >&2; exit 1; }; ARCHES=("$2"); shift 2 ;;
    *) echo "usage: bash scripts/deb/publish.sh [--pool <dir>] [--arch <amd64|arm64>]" >&2; exit 1 ;;
    esac
done
for a in "${ARCHES[@]}"; do
    case "${a}" in amd64 | arm64) ;; *) echo "error: --arch must be amd64 or arm64" >&2; exit 1 ;; esac
done
for t in curl sha256sum python3 git jq; do
    command -v "${t}" >/dev/null 2>&1 || { echo "error: ${t} is required and not on PATH" >&2; exit 1; }
done

registry_load
registry_repo_name
registry_token --write

[ -z "$(git -C "${REPO_ROOT}" status --porcelain)" ] || {
    echo "error: ${REPO_ROOT} has uncommitted changes. An archive is published as the output of one commit; commit first, rebuild, then publish" >&2
    exit 1
}
HEAD_COMMIT="$(git -C "${REPO_ROOT}" rev-parse HEAD)"
HEAD_CREATED="$(git -C "${REPO_ROOT}" show -s --format=%cI HEAD)"
TAG="$(release_tag "${HEAD_COMMIT}")"
FIELDS="python3 ${HERE}/control-fields.py"

WORK="$(mktemp -d)"
trap 'rm -rf "${WORK}"' EXIT
artifact_annotations "${REPO_NAME}" "${HEAD_COMMIT}" "${HEAD_CREATED}" "${WORK}/annotations.json"

published=0
present=0
ROWS=()
declare -A ROW_SEEN=()
for a in "${ARCHES[@]}"; do
    pool="${POOL_ROOT}/${a}/pool"
    [ -d "${pool}" ] || { echo "error: ${pool} does not exist; build the pool first (make os-debs)" >&2; exit 1; }
    DEBS=()
    while IFS= read -r f; do DEBS+=("${f}"); done < <(find "${pool}" -maxdepth 1 -type f -name '*.deb' | LC_ALL=C sort)
    [ "${#DEBS[@]}" -gt 0 ] || { echo "error: no .deb under ${pool}; nothing to publish" >&2; exit 1; }

    # Refusals first, so a run publishes all or nothing.
    for deb in "${DEBS[@]}"; do
        n="$(basename "${deb}")"
        mapfile -t got < <(${FIELDS} "${deb}" Version Mica-Source-Repo Mica-Source-Commit)
        version="${got[0]:-}"; repo="${got[1]:-}"; commit="${got[2]:-}"
        case "${version}" in
        *.dirty-*) echo "error: ${n} is versioned ${version}: a dirty archive is one no commit reproduces, and an artifact holds only what a lock can name. Commit, rebuild, publish" >&2; exit 1 ;;
        esac
        [ "${repo}" = "${REPO_NAME}" ] || {
            echo "error: ${n} says Mica-Source-Repo: ${repo:-(none)}, and this checkout is ${REPO_NAME}. Only this repository's own archives are published under its artifacts" >&2
            exit 1
        }
        [ "${commit}" = "${HEAD_COMMIT}" ] || {
            echo "error: ${n} says Mica-Source-Commit: ${commit:-(none)}, and HEAD is ${HEAD_COMMIT}. It was built from another commit; publish from that checkout, or rebuild here" >&2
            exit 1
        }
    done

    artifact="$(pool_repo "${REPO_NAME}")"; ref="$(pool_tag "${a}" "${TAG}")"
    jq --arg a "${a}" '. + {"mica.arch": $a}' "${WORK}/annotations.json" >"${WORK}/annotations-${a}.json"
    : >"${WORK}/layers-${a}.tsv"
    for deb in "${DEBS[@]}"; do
        printf '%s\t%s\t%s\n' "${deb}" application/vnd.mica.deb "$(basename "${deb}")" >>"${WORK}/layers-${a}.tsv"
    done

    # The tag, if it exists, must be exactly this pool: a name that held other
    # bytes is a lock that could never be right.
    status="$(oci_manifest_get "${artifact}" "${ref}" "${WORK}/existing-${a}.json")"
    case "${status}" in
    200)
        for deb in "${DEBS[@]}"; do
            n="$(basename "${deb}")"; sha="sha256:$(sha256sum "${deb}" | cut -d' ' -f1)"
            have="$(jq -r --arg t "${n}" '.layers[] | select(.annotations["org.opencontainers.image.title"] == $t) | .digest' "${WORK}/existing-${a}.json")"
            [ "${have}" = "${sha}" ] || { echo "error: ${OCI_HOST}/${artifact}:${ref} exists and carries ${n} at ${have:-nothing}, and the archive here is ${sha}. Under one name the registry holds different bytes than this build produced; nothing was published" >&2; exit 1; }
        done
        present=$((present + ${#DEBS[@]}))
        echo "publish.sh: ${OCI_HOST}/${artifact}:${ref} exists with these ${#DEBS[@]} archive(s); comparing bytes"
        ;;
    404)
        digest="$(oci_push "${artifact}" "${ref}" application/vnd.mica.pool "${WORK}/annotations-${a}.json" "${WORK}/layers-${a}.tsv")" || exit 1
        published=$((published + ${#DEBS[@]}))
        echo "publish.sh: ${#DEBS[@]} archive(s) pushed as ${OCI_HOST}/${artifact}:${ref} (${digest})"
        ;;
    401 | 403) echo "error: the registry answered ${status} for ${OCI_HOST}/${artifact}; ${MICA_RELEASE_TOKEN_VAR} does not grant access" >&2; exit 1 ;;
    000) echo "error: ${OCI_HOST} could not be reached (transport failure)" >&2; exit 1 ;;
    *) echo "error: reading ${OCI_HOST}/${artifact}:${ref} answered HTTP ${status}" >&2; exit 1 ;;
    esac

    # Public, always: a private package is a consumer's 401 later.
    oci_require_public "${artifact}" "${ref}" || exit 1
    # Read back, always.
    for deb in "${DEBS[@]}"; do
        n="$(basename "${deb}")"
        sha="$(sha256sum "${deb}" | cut -d' ' -f1)"
        status="$(oci_blob_get "${artifact}" "sha256:${sha}" "${WORK}/back.deb")"
        [ "${status}" = 200 ] || { echo "error: reading ${n} back from ${OCI_HOST}/${artifact} answered HTTP ${status}; the registry does not serve what it accepted" >&2; exit 1; }
        got="$(sha256sum "${WORK}/back.deb" | cut -d' ' -f1)"
        [ "${got}" = "${sha}" ] || { echo "error: the registry serves ${n} with sha256 ${got}, and the archive here is ${sha}" >&2; exit 1; }
        mapfile -t got < <(${FIELDS} "${deb}" Package Version Architecture)
        row="${got[0]}	${got[1]}	${got[2]}	${sha}	${REPO_NAME}	${HEAD_COMMIT}"
        [ -n "${ROW_SEEN[${row}]:-}" ] || { ROW_SEEN["${row}"]=1; ROWS+=("${row}"); }
    done
done

echo "publish.sh: ${published} archive(s) pushed, ${present} already present, all read back at their digests; ${OCI_HOST}/$(pool_repo "${REPO_NAME}"):pool.<arch>.${TAG}"
echo "publish.sh: lock rows for what those artifacts hold from this commit:"
printf '%s\n' "${ROWS[@]}"

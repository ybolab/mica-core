#!/usr/bin/env bash
# The lock's only writer, and its reader for every other script.
#
#   bash build-env/deb/lock.sh --rows [--arch <amd64|arm64>]
#       print the validated pins as rows (all, or the ones that pool holds)
#   bash build-env/deb/lock.sh --bump <component> [--tag <build-...>] [--version <v>] [--package <p> ...]
#       rewrite <component>'s rows from one of its pool artifacts and print the diff
#
# --bump reads the --tag artifacts (else the newest build-<commit12>, by the
# created annotation of each tag's manifest) of <component> for both
# architectures, and writes one pin per .deb layer from the archive's own
# control fields and digest, replacing that component's pins only.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${HERE}/../.." && pwd)"
# shellcheck disable=SC1091
. "${HERE}/registry.sh"

MODE=""
ARCH=""
COMPONENT=""
TAG=""
VERSION=""
PACKAGES=()
USAGE="usage: bash build-env/deb/lock.sh --rows [--arch <a>] | --bump <component> [--tag <build-...>] [--version <v>] [--package <p> ...]"
while [ "$#" -gt 0 ]; do
    case "$1" in
    --rows) MODE=rows; shift ;;
    --arch) ARCH="${2-}"; [ -n "${ARCH}" ] || { echo "error: --arch takes amd64 or arm64" >&2; exit 1; }; shift 2 ;;
    --bump) MODE=bump; COMPONENT="${2-}"; [ -n "${COMPONENT}" ] || { echo "error: --bump takes a component (source repository) name" >&2; exit 1; }; shift 2 ;;
    --tag) TAG="${2-}"; [ -n "${TAG}" ] || { echo "error: --tag takes a release tag (build-<commit12>)" >&2; exit 1; }; shift 2 ;;
    --version) VERSION="${2-}"; [ -n "${VERSION}" ] || { echo "error: --version takes a Debian version" >&2; exit 1; }; shift 2 ;;
    --package) [ -n "${2-}" ] || { echo "error: --package takes a package name" >&2; exit 1; }; PACKAGES+=("$2"); shift 2 ;;
    *) echo "${USAGE}" >&2; exit 1 ;;
    esac
done
[ -n "${MODE}" ] || { echo "${USAGE}" >&2; exit 1; }

registry_load
if [ "${MODE}" = rows ]; then
    case "${ARCH}" in '' | amd64 | arm64) ;; *) echo "error: --arch must be amd64 or arm64" >&2; exit 1 ;; esac
    lock_rows "${ARCH}"
    exit 0
fi

[[ "${COMPONENT}" =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ ]] || { echo "error: '${COMPONENT}' is not a component name" >&2; exit 1; }
for t in curl sha256sum python3; do
    command -v "${t}" >/dev/null 2>&1 || { echo "error: ${t} is required and not on PATH" >&2; exit 1; }
done
lock_rows >/dev/null   # the current pins must already be well formed
registry_token

WORK="$(mktemp -d)"
trap 'rm -rf "${WORK}"' EXIT

# Where the component's pool artifacts are: its own package (own), else the
# shared package its artifacts went to before per-repository packages (legacy).
OWN_ARTIFACT="$(pool_repo "${COMPONENT}")" || exit 1
artifact_of() { case "$1" in own) printf '%s' "${OWN_ARTIFACT}" ;; legacy) legacy_pool_repo ;; esac; }
ref_of() { case "$1" in own) pool_tag "$2" "$3" ;; legacy) legacy_pool_tag "${COMPONENT}" "$2" "$3" ;; esac; } # <loc> <arch> <build-tag>
prefix_of() { case "$1" in own) printf 'pool.%s' "$2" ;; legacy) printf '%s.%s' "${COMPONENT}" "$2" ;; esac; } # <loc> <arch>

# The tag: named, or the newest build-* one either architecture's pool carries.
LOC=""
if [ -z "${TAG}" ]; then
    for loc in own legacy; do
        : >"${WORK}/newest"
        for a in amd64 arm64; do
            t="$(newest_tag "$(artifact_of "${loc}")" "$(prefix_of "${loc}" "${a}")")" || exit 1
            [ -z "${t}" ] || printf '%s\n' "${t}" >>"${WORK}/newest"
        done
        [ ! -s "${WORK}/newest" ] || { LOC="${loc}"; break; }
    done
    [ -n "${LOC}" ] || { echo "error: ${OCI_HOST}/${OWN_ARTIFACT} has no pool.<arch>.build-<commit12> artifact (nor ${OCI_HOST}/$(legacy_pool_repo) a ${COMPONENT}.<arch>.build-<commit12>); nothing to lock" >&2; exit 1; }
    # Two architectures published from different commits: the newer wins,
    # and the older one's archives are not locked (a bump is half done).
    TAG="$(sort -u "${WORK}/newest" | tail -n1)"
    if [ "$(sort -u "${WORK}/newest" | wc -l)" -gt 1 ]; then
        best=""; best_created=""
        while IFS= read -r t; do
            for a in amd64 arm64; do
                [ "$(oci_manifest_get "$(artifact_of "${LOC}")" "$(ref_of "${LOC}" "${a}" "${t}")" "${WORK}/m.json")" = 200 ] || continue
                created="$(jq -r '.annotations["org.opencontainers.image.created"] // empty' "${WORK}/m.json")"
                if [ -z "${best}" ] || [[ "${created}" > "${best_created}" ]]; then best="${t}"; best_created="${created}"; fi
            done
        done < <(sort -u "${WORK}/newest")
        TAG="${best}"
    fi
fi
[[ "${TAG}" =~ ^build-[0-9a-f]{12}$ ]] || { echo "error: the tag '${TAG}' is not build-<commit12>; only per-commit artifacts are locked" >&2; exit 1; }
if [ -z "${LOC}" ]; then
    # The shared package is asked only after the own package answered 404.
    for loc in own legacy; do
        for a in amd64 arm64; do
            status="$(oci_manifest_get "$(artifact_of "${loc}")" "$(ref_of "${loc}" "${a}" "${TAG}")" "${WORK}/m.json")"
            case "${status}" in
            200) LOC="${loc}"; break 2 ;;
            404) ;;
            *) echo "error: reading ${OCI_HOST}/$(artifact_of "${loc}"):$(ref_of "${loc}" "${a}" "${TAG}") answered HTTP ${status}; nothing else is asked" >&2; exit 1 ;;
            esac
        done
    done
    [ -n "${LOC}" ] || { echo "error: ${OCI_HOST}/${OWN_ARTIFACT} has no pool.<arch>.${TAG} (nor ${OCI_HOST}/$(legacy_pool_repo) a ${COMPONENT}.<arch>.${TAG})" >&2; exit 1; }
fi

# Every .deb layer of both architectures' artifacts, downloaded and read: the
# row is what the archive says.
: >"${WORK}/rows.tsv"
FIELDS="python3 ${HERE}/control-fields.py"
declare -A FOUND=()
declare -A ROW_SEEN=()
artifacts=0
for a in amd64 arm64; do
    artifact="$(artifact_of "${LOC}")"; ref="$(ref_of "${LOC}" "${a}" "${TAG}")"
    status="$(oci_manifest_get "${artifact}" "${ref}" "${WORK}/manifest-${a}.json")"
    case "${status}" in
    200) ;;
    404) continue ;;
    401 | 403) echo "error: reading ${OCI_HOST}/${artifact} answered ${status}; ${MICA_RELEASE_TOKEN_VAR} does not grant read access" >&2; exit 1 ;;
    000) echo "error: ${OCI_HOST} could not be reached (transport failure)" >&2; exit 1 ;;
    *) echo "error: reading ${OCI_HOST}/${artifact}:${ref} answered HTTP ${status}" >&2; exit 1 ;;
    esac
    artifacts=$((artifacts + 1))
    manifest_commit="$(jq -r '.annotations["org.opencontainers.image.revision"] // empty' "${WORK}/manifest-${a}.json")"
    [ "${TAG}" = "$(release_tag "${manifest_commit}")" ] || { echo "error: ${OCI_HOST}/${artifact}:${ref} says it was built from '${manifest_commit}', which is not the commit its tag names" >&2; exit 1; }
    [ "$(jq -r '.artifactType // empty' "${WORK}/manifest-${a}.json")" = application/vnd.mica.pool ] || { echo "error: ${OCI_HOST}/${artifact}:${ref} is a '$(jq -r '.artifactType // "(none)"' "${WORK}/manifest-${a}.json")' artifact, not application/vnd.mica.pool" >&2; exit 1; }
    [ "$(jq -r '.annotations["mica.source-repo"] // empty' "${WORK}/manifest-${a}.json")" = "${COMPONENT}" ] || { echo "error: ${OCI_HOST}/${artifact}:${ref} says mica.source-repo='$(jq -r '.annotations["mica.source-repo"] // ""' "${WORK}/manifest-${a}.json")', not ${COMPONENT}; a package holds only its own repository's artifacts" >&2; exit 1; }
    while IFS=$'\t' read -r name digest; do
        [ -n "${name}" ] || continue
        pkg="${name%%_*}"
        if [ "${#PACKAGES[@]}" -gt 0 ]; then
            wanted=0
            for p in "${PACKAGES[@]}"; do [ "${p}" != "${pkg}" ] || wanted=1; done
            [ "${wanted}" = 1 ] || continue
        fi
        tmp="${WORK}/${name}"
        status="$(oci_blob_get "${artifact}" "${digest}" "${tmp}")"
        [ "${status}" = 200 ] || { echo "error: downloading ${name} (${digest}) from ${OCI_HOST}/${artifact} answered HTTP ${status}" >&2; exit 1; }
        sha="$(sha256sum "${tmp}" | cut -d' ' -f1)"
        [ "${digest}" = "sha256:${sha}" ] || { echo "error: ${name}: the manifest says ${digest} and the bytes hash to sha256:${sha}; the registry served other bytes" >&2; exit 1; }
        mapfile -t got < <(${FIELDS} "${tmp}" Package Version Architecture Mica-Source-Repo Mica-Source-Commit)
        p="${got[0]:-}"; v="${got[1]:-}"; ar="${got[2]:-}"; repo="${got[3]:-}"; commit="${got[4]:-}"
        [ "${name}" = "${p}_${v}_${ar}.deb" ] || { echo "error: the layer ${name} says Package: ${p}, Version: ${v}, Architecture: ${ar} inside; a name that does not match its control file is refused" >&2; exit 1; }
        [ "${repo}" = "${COMPONENT}" ] || { echo "error: ${name} in ${COMPONENT}'s artifact says Mica-Source-Repo: ${repo:-(none)}; an artifact holds only its own repository's archives" >&2; exit 1; }
        [[ "${commit}" =~ ^[0-9a-f]{40}$ ]] && [ "${TAG}" = "$(release_tag "${commit}")" ] || { echo "error: ${name} says Mica-Source-Commit: ${commit:-(none)}, which is not the commit the tag ${TAG} names" >&2; exit 1; }
        [ -z "${VERSION}" ] || [ "${v}" = "${VERSION}" ] || continue
        case "${v}" in *.dirty-*) echo "error: ${name} is a dirty archive in an artifact; it cannot be locked" >&2; exit 1 ;; esac
        FOUND["${pkg}"]=1
        row="$(printf '%s\t%s\t%s\t%s\t%s\t%s' "${p}" "${v}" "${ar}" "${sha}" "${COMPONENT}" "${commit}")"
        [ -n "${ROW_SEEN[${row}]:-}" ] || { ROW_SEEN["${row}"]=1; printf '%s\n' "${row}" >>"${WORK}/rows.tsv"; }
    done < <(jq -r '.layers[] | select(.mediaType == "application/vnd.mica.deb") | [.annotations["org.opencontainers.image.title"], .digest] | @tsv' "${WORK}/manifest-${a}.json")
done
[ "${artifacts}" -gt 0 ] || { echo "error: ${OCI_HOST}/$(artifact_of "${LOC}") has no $(prefix_of "${LOC}" '<arch>').${TAG}" >&2; exit 1; }
for p in ${PACKAGES[@]+"${PACKAGES[@]}"}; do
    [ -n "${FOUND[${p}]:-}" ] || { echo "error: the artifacts ${TAG} of ${COMPONENT} hold no archive named ${p}${VERSION:+ at ${VERSION}}" >&2; exit 1; }
done
[ -s "${WORK}/rows.tsv" ] || { echo "error: the artifacts ${TAG} of ${COMPONENT} hold no archive${VERSION:+ at version ${VERSION}}; nothing to lock" >&2; exit 1; }

# Rebuilt in scratch, validated, then swapped in. An `all` archive serves both pools.
NEW="${WORK}/pins"; mkdir -p "${NEW}"
cp -a "${LOCK_DIR}/." "${NEW}/"
while IFS=$'\t' read -r p v a sha repo commit; do
    [ -n "${p}" ] || continue
    if [ "${a}" = all ]; then pools="amd64 arm64"; else pools="${a}"; fi
    asset="${p}_${v}_${a}.deb"
    file="${NEW}/${p}.json"
    [ -f "${file}" ] && [ "$(jq -r '.repository + " " + .commit' "${file}")" = "${repo} ${commit}" ] || printf '{"name":"%s","repository":"%s","commit":"%s","targets":{}}\n' "${p}" "${repo}" "${commit}" >"${file}"
    for pool in ${pools}; do
        jq --arg pool "${pool}" --arg v "${v}" --arg a "${a}" --arg sha "${sha}" --arg asset "${asset}" \
            '.targets[$pool] = {version: $v, architecture: $a, sha256: $sha, asset: $asset}' "${file}" >"${file}.tmp" && mv "${file}.tmp" "${file}"
    done
done <"${WORK}/rows.tsv"
# The component's packages this artifact no longer provides go, unless the bump was narrowed.
if [ "${#PACKAGES[@]}" -eq 0 ]; then
    for file in "${NEW}"/*.json; do
        [ -e "${file}" ] || continue
        [ "$(jq -r '.repository' "${file}")" != "${COMPONENT}" ] || grep -q "^$(basename "${file}" .json)	" "${WORK}/rows.tsv" || rm -f "${file}"
    done
fi
LOCK_DIR="${NEW}" lock_rows >/dev/null
if diff -ruN --label "a/${LOCK_DIR#"${REPO_ROOT}"/}" --label "b/${LOCK_DIR#"${REPO_ROOT}"/}" "${LOCK_DIR}" "${NEW}"; then
    echo "lock.sh: ${LOCK_DIR#"${REPO_ROOT}"/} already pins what ${COMPONENT} publishes at ${TAG}; no change"
    exit 0
fi
rm -f "${LOCK_DIR}"/*.json
cp -a "${NEW}/." "${LOCK_DIR}/"
echo "lock.sh: ${LOCK_DIR#"${REPO_ROOT}"/} rewritten for ${COMPONENT} at ${TAG}; review the diff above, then \`make os-pool\`"

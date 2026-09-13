#!/usr/bin/env bash
# Whether this commit builds and publishes a pool, or needs none.
#
#   bash scripts/build/pool-decision.sh [--git <dir>] [--head <rev>]
#
# Prints the baseline and the reason, and writes pool=build or pool=skip to
# $GITHUB_OUTPUT when that is set. It always exits 0: anything it cannot
# establish -- a failed read, a missing or partial baseline, a pin that does
# not verify, an error of its own -- is pool=build, and the full build path
# fetches, verifies and gates exactly as it did before this step existed.
#
# pool=skip needs all of:
#   - a baseline: the ancestor of HEAD nearest to it whose pool this
#     repository published for BOTH amd64 and arm64, each manifest an
#     application/vnd.mica.pool of this repository at that same full commit
#     (revision and mica.source-commit), mica.arch naming its architecture,
#     and only application/vnd.mica.deb layers titled for that architecture;
#   - every path changed between the baseline and HEAD in the allow-list of
#     paths no package is built from: docs/**, any *.md, .gitignore and
#     deps/build-env.json;
#   - when deps/build-env.json changed, both pins verified against their
#     releases (scripts/build/build-env.sh) and IMAGE_MICA_BUILD_RUST and
#     IMAGE_MICA_BUILD_BASE, the only images this repository builds in,
#     the same digest pins in both images.env. Equal version strings prove
#     nothing and are not compared.
# A skipped commit has no pool of its own; the baseline's pool, built from
# the same package inputs, carries its own provenance and is not repacked.
#
# The registry is read anonymously with scripts/deb/oci.sh.
# MICA_POOL_DECISION_REGISTRY_DIR replaces it with a fixture directory for
# scripts/gate/pool-decision-test.sh: tags (one per line), manifests/<tag>.json
# and optionally status/<tag> or status/tags holding an HTTP status.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${HERE}/../.." && pwd)"
GIT_DIR_ARG="${REPO_ROOT}"
HEAD_REV=HEAD
DECIDE=0
while [ "$#" -gt 0 ]; do
    case "$1" in
    --git) GIT_DIR_ARG="${2:?--git takes a directory}"; shift 2 ;;
    --head) HEAD_REV="${2:?--head takes a revision}"; shift 2 ;;
    --decide) DECIDE=1; shift ;;
    *) echo "usage: bash scripts/build/pool-decision.sh [--git <dir>] [--head <rev>]" >&2; exit 2 ;;
    esac
done

# The outer run: the decision runs as its own process under set -e, and only
# its explicit last line can say skip.
if [ "${DECIDE}" = 0 ]; then
    out="$(mktemp)"
    rc=0
    bash "${BASH_SOURCE[0]}" --decide --git "${GIT_DIR_ARG}" --head "${HEAD_REV}" >"${out}" || rc=$?
    grep -v '^decision=' "${out}" || true
    last="$(tail -n1 "${out}")"
    rm -f "${out}"
    decision=build
    reason="the decision step failed (exit ${rc}) before it reached a decision"
    if [ "${rc}" = 0 ]; then
        case "${last}" in
        decision=skip\ *) decision=skip; reason="${last#decision=skip }" ;;
        decision=build\ *) reason="${last#decision=build }" ;;
        *) reason="the decision step ended without a decision" ;;
        esac
    fi
    echo "pool-decision: ${decision}: ${reason}"
    [ -z "${GITHUB_OUTPUT:-}" ] || echo "pool=${decision}" >>"${GITHUB_OUTPUT}"
    exit 0
fi

build() { echo "decision=build $*"; exit 0; }
skip() { echo "decision=skip $*"; exit 0; }
g() { git -C "${GIT_DIR_ARG}" "$@"; }

for t in git jq curl sha256sum; do
    command -v "${t}" >/dev/null 2>&1 || build "${t} is not on PATH"
done
HEAD_COMMIT="$(g rev-parse --verify --quiet "${HEAD_REV}^{commit}")" || build "${HEAD_REV} is not a commit"

# ---- the registry, or its fixture ----
if [ -n "${MICA_POOL_DECISION_REGISTRY_DIR:-}" ]; then
    FIX="${MICA_POOL_DECISION_REGISTRY_DIR}"
    REPO_NAME="${MICA_POOL_DECISION_REPO_NAME:-mica-core}"
    ARTIFACT="fixture/${REPO_NAME}"
    list_tags() {
        [ ! -f "${FIX}/status/tags" ] || { echo "error: listing tags answered HTTP $(cat "${FIX}/status/tags")" >&2; return 1; }
        [ ! -f "${FIX}/tags" ] || cat "${FIX}/tags"
    }
    get_manifest() { # <tag> <out> -> status
        if [ -f "${FIX}/status/$1" ]; then cat "${FIX}/status/$1"
        elif [ -f "${FIX}/manifests/$1.json" ]; then cp "${FIX}/manifests/$1.json" "$2"; printf 200
        else printf 404; fi
    }
else
    # shellcheck disable=SC1091
    . "${REPO_ROOT}/scripts/deb/registry.sh"
    registry_load || build "registry.env did not load"
    registry_repo_name || build "the repository name could not be derived"
    ARTIFACT="$(pool_repo "${REPO_NAME}")"
    REGISTRY_TOKEN=""
    list_tags() { oci_tags "${ARTIFACT}"; }
    get_manifest() { oci_manifest_get "${ARTIFACT}" "$1" "$2"; }
fi

WORK="$(mktemp -d)"
trap 'rm -rf "${WORK}"' EXIT

# ---- the baseline ----
list_tags >"${WORK}/tags" || build "the tags of ${ARTIFACT} could not be listed, so no baseline can be established"
# Commits published under both architecture tags; a one-architecture upload is not a pool.
sed -n 's/^pool\.amd64\.build-\([0-9a-f]\{12\}\)$/\1/p' "${WORK}/tags" | LC_ALL=C sort -u >"${WORK}/amd64"
sed -n 's/^pool\.arm64\.build-\([0-9a-f]\{12\}\)$/\1/p' "${WORK}/tags" | LC_ALL=C sort -u >"${WORK}/arm64"
: >"${WORK}/candidates"
while IFS= read -r c12; do
    full="$(g rev-parse --verify --quiet "${c12}^{commit}" 2>/dev/null)" || continue
    [ "${full:0:12}" = "${c12}" ] || continue
    g merge-base --is-ancestor "${full}" "${HEAD_COMMIT}" || continue
    printf '%s %s\n' "$(g rev-list --count "${full}")" "${full}" >>"${WORK}/candidates"
done < <(LC_ALL=C comm -12 "${WORK}/amd64" "${WORK}/arm64")
[ -s "${WORK}/candidates" ] || build "no ancestor of ${HEAD_COMMIT:0:12} has a pool published for both amd64 and arm64 (first publication)"

BASE=""
checked=0
while read -r _count full; do
    checked=$((checked + 1))
    [ "${checked}" -le 10 ] || build "none of the 10 nearest published ancestors is a complete pool of this repository"
    ok=1
    for arch in amd64 arm64; do
        tag="pool.${arch}.build-${full:0:12}"
        status="$(get_manifest "${tag}" "${WORK}/m.json")"
        case "${status}" in
        200) ;;
        404) echo "baseline candidate ${full:0:12}: ${tag} is listed and answers 404; not a pool"; ok=0; break ;;
        *) build "reading ${tag} answered HTTP ${status}, so the baseline is uncertain" ;;
        esac
        if ! jq -e --arg repo "${REPO_NAME}" --arg commit "${full}" --arg arch "${arch}" '
            .artifactType == "application/vnd.mica.pool"
            and .annotations["mica.source-repo"] == $repo
            and .annotations["org.opencontainers.image.revision"] == $commit
            and .annotations["mica.source-commit"] == $commit
            and .annotations["mica.arch"] == $arch
            and (.layers | type == "array" and length > 0)
            and all(.layers[]; .mediaType == "application/vnd.mica.deb"
                and ((.annotations["org.opencontainers.image.title"] // "")
                     | test("^[a-z0-9][a-z0-9+.-]*_[^_/]+_(" + $arch + "|all)\\.deb$")))
            ' "${WORK}/m.json" >/dev/null 2>&1; then
            echo "baseline candidate ${full:0:12}: ${tag} is not a ${arch} pool of ${REPO_NAME} at ${full}; not a baseline"
            ok=0
            break
        fi
    done
    if [ "${ok}" = 1 ]; then BASE="${full}"; break; fi
done < <(sort -k1,1nr -k2,2 "${WORK}/candidates")
[ -n "${BASE}" ] || build "no published ancestor of ${HEAD_COMMIT:0:12} is a complete pool of this repository"
echo "baseline: ${BASE} (pool.amd64 and pool.arm64 of ${REPO_NAME}); head: ${HEAD_COMMIT}"

# ---- what changed ----
g diff --no-renames --name-only -z "${BASE}" "${HEAD_COMMIT}" >"${WORK}/changed"
PIN_CHANGED=0
n=0
while IFS= read -r -d '' path; do
    n=$((n + 1))
    case "${path}" in
    deps/build-env.json) PIN_CHANGED=1 ;;
    docs/* | *.md | .gitignore) ;;
    *) build "${path} changed since ${BASE:0:12} and is a package, build or workflow input" ;;
    esac
done <"${WORK}/changed"
[ "${n}" -gt 0 ] || skip "nothing changed since ${BASE:0:12}, whose pool is published"
[ "${PIN_CHANGED}" = 1 ] || skip "only docs/markdown/.gitignore changed since ${BASE:0:12} (${n} path(s))"

# ---- the release pin: effective images, not version strings ----
image_of() { # <images.env> <key>
    local v
    [ "$(grep -c "^$2=" "$1")" = 1 ] || return 1
    v="$(sed -n "s/^$2=//p" "$1")"
    [[ "${v}" =~ ^[a-z0-9][a-z0-9._/-]*:[A-Za-z0-9._-]+@sha256:[0-9a-f]{64}$ ]] || return 1
    printf '%s' "${v}"
}
for side in base head; do
    rev="${BASE}"
    [ "${side}" = base ] || rev="${HEAD_COMMIT}"
    g show "${rev}:deps/build-env.json" >"${WORK}/${side}.json" 2>/dev/null ||
        build "deps/build-env.json does not exist at ${rev:0:12}, so its images cannot be compared"
    MICA_BUILD_ENV_PIN="${WORK}/${side}.json" MICA_BUILD_ENV_DIR="${WORK}/${side}-env" \
        bash "${REPO_ROOT}/scripts/build/build-env.sh" fetch >&2 ||
        build "the release pin at ${rev:0:12} did not verify, so its images are unknown"
done
for key in IMAGE_MICA_BUILD_RUST IMAGE_MICA_BUILD_BASE; do
    a="$(image_of "${WORK}/base-env/images.env" "${key}")" || build "${key} is missing or not a digest pin in the release at ${BASE:0:12}"
    b="$(image_of "${WORK}/head-env/images.env" "${key}")" || build "${key} is missing or not a digest pin in the release at ${HEAD_COMMIT:0:12}"
    [ "${a}" = "${b}" ] || build "${key} changed: ${a} -> ${b}"
done
skip "only docs/markdown/.gitignore and the release pin changed since ${BASE:0:12}, and both verified releases name the same IMAGE_MICA_BUILD_RUST and IMAGE_MICA_BUILD_BASE"

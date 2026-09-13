#!/usr/bin/env bash
# Source dependencies by pin: fetch, check, bump and publish.
#
#   bash tools/deps.sh fetch [--check]              every deps/sources/*.json into its path
#   bash tools/deps.sh bump <repository> [--tag build-<commit12>] [--path <dir>]
#   bash tools/deps.sh publish-source               this repository's HEAD as a release asset
#
# A source dependency is a pin, one JSON file per repository under
# deps/sources/, in the shape of the Debian pins under rootfs/debian/packages:
#
#   { "name": "mica-build-env", "repository": "mica-build-env",
#     "commit": "<40 hex>", "path": "build-env",
#     "asset": "mica-build-env-<commit12>.tar.gz", "sha256": "<64 hex>" }
#
# The asset is the `git archive` of that commit, published as the release
# `build-<commit12>` of ybolab/<repository> (by the repository's workflow, or
# by `publish-source` from a developer machine). `fetch` downloads it,
# verifies the sha256, replaces <path> with its contents and records the pin
# in <path>/.deps-pin, so a second fetch is a no-op and the lineage record
# can require the checkout to match the pin. <path> is gitignored: what is
# there is the dependency at the pin, never edited in place -- a change is a
# commit in the dependency, a release, and a bump here.
#
# This file is the same in every Mica OS repository, VENDORED rather than
# fetched, because it is what fetches everything else: bash, curl, jq, tar
# and sha256sum only. The API, organisation and token variable are the
# defaults below; MICA_DEPS_API, MICA_DEPS_UPLOAD, MICA_DEPS_OWNER and
# MICA_DEPS_TOKEN_VAR override them (the tests drive a stub that way), and the
# token falls back to `gh auth token`. Nothing prints the token.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${HERE}/.." && pwd)"
API="${MICA_DEPS_API:-https://api.github.com}"
UPLOAD="${MICA_DEPS_UPLOAD:-https://uploads.github.com}"
OWNER="${MICA_DEPS_OWNER:-ybolab}"
TOKEN_VAR="${MICA_DEPS_TOKEN_VAR:-GH_TOKEN}"
PINS="${MICA_DEPS_DIR:-${REPO_ROOT}/deps/sources}"

die() { echo "deps.sh: error: $*" >&2; exit 1; }
for t in curl jq tar sha256sum git; do
    command -v "${t}" >/dev/null 2>&1 || die "${t} is required and not on PATH"
done
case "${API}" in https://* | http://127.0.0.1:* | http://localhost:*) ;; *) die "MICA_DEPS_API='${API}' is not https; the token would be sent in clear" ;; esac

token() {
    TOKEN="${!TOKEN_VAR:-}"
    if [ -z "${TOKEN}" ] && [ -z "${MICA_DEPS_NO_GH:-}" ] && command -v gh >/dev/null 2>&1; then
        TOKEN="$(gh auth token 2>/dev/null || true)"
    fi
    [ -n "${TOKEN}" ] || die "${TOKEN_VAR} is unset or empty and \`gh auth token\` gave nothing. The releases of ${OWNER} are private; export the token in ${TOKEN_VAR} or log in with gh"
}
api() { # method url out [curl args] -> status
    local method="$1" url="$2" out="$3"; shift 3
    curl -sS --max-time 600 -o "${out}" -w '%{http_code}' -H "Authorization: Bearer ${TOKEN}" \
        -H 'Accept: application/vnd.github+json' -H 'X-GitHub-Api-Version: 2022-11-28' -X "${method}" "$@" "${url}" 2>/dev/null || echo 000
}
download() { # asset url, out -> status (the redirect to object storage is taken without the token)
    local status redirect
    read -r status redirect < <(curl -sS --max-time 600 -o "$2" -w '%{http_code} %{redirect_url}\n' -H "Authorization: Bearer ${TOKEN}" \
        -H 'Accept: application/octet-stream' -H 'X-GitHub-Api-Version: 2022-11-28' "$1" 2>/dev/null || echo '000 ')
    case "${status}" in
    200) echo 200 ;;
    301 | 302 | 307 | 308) [ -n "${redirect}" ] && curl -sS -L --max-time 1800 -o "$2" -w '%{http_code}' "${redirect}" 2>/dev/null || echo 000 ;;
    *) echo "${status}" ;;
    esac
}
release() { # repo tag out -> status
    api GET "${API}/repos/${OWNER}/$1/releases/tags/$2" "$3"
}
explain() { # status what
    case "$1" in
    401 | 403) die "$2 answered $1; ${TOKEN_VAR} does not grant access to ${OWNER}" ;;
    000) die "$2: ${API} could not be reached (transport failure)" ;;
    esac
}
pin_fields() { # file -> name repository commit path asset sha256 (validated)
    jq -e 'type == "object" and (keys | sort == ["asset","commit","name","path","repository","sha256"])
        and (.name | test("^[A-Za-z0-9][A-Za-z0-9._-]*$")) and (.repository | test("^[A-Za-z0-9][A-Za-z0-9._-]*$"))
        and (.commit | test("^[0-9a-f]{40}$")) and (.sha256 | test("^[0-9a-f]{64}$"))
        and (.path | test("^[A-Za-z0-9][A-Za-z0-9/._-]*$") and (contains("..") | not))
        and (.asset | test("^[A-Za-z0-9][A-Za-z0-9._-]*\\.tar\\.gz$"))' "$1" >/dev/null 2>&1 ||
        die "$1 is not a source pin: an object with exactly name, repository, commit (40 hex), path (relative), asset (*.tar.gz) and sha256 (64 hex)"
    jq -r '[.name, .repository, .commit, .path, .asset, .sha256] | @tsv' "$1"
}

cmd_fetch() {
    local check=0
    [ "${1:-}" != --check ] || check=1
    [ -d "${PINS}" ] || { echo "deps.sh: ${PINS#"${REPO_ROOT}"/} does not exist; no source dependency to fetch"; return 0; }
    mapfile -t files < <(find "${PINS}" -maxdepth 1 -type f -name '*.json' | LC_ALL=C sort)
    [ "${#files[@]}" -gt 0 ] || { echo "deps.sh: no pin under ${PINS#"${REPO_ROOT}"/}; nothing to fetch"; return 0; }
    token
    local work; work="$(mktemp -d)"; trap 'rm -rf "${work}"' RETURN
    for f in "${files[@]}"; do
        IFS=$'\t' read -r name repository commit path asset sha < <(pin_fields "${f}")
        local dest="${REPO_ROOT}/${path}" tag="build-${commit:0:12}"
        if [ "${check}" = 0 ] && [ -f "${dest}/.deps-pin" ] && [ "$(cat "${dest}/.deps-pin")" = "${sha}" ]; then
            echo "deps.sh: ${path}/ is ${name} at ${commit:0:12} already"
            continue
        fi
        local status
        status="$(release "${repository}" "${tag}" "${work}/release.json")"
        explain "${status}" "reading the release ${tag} of ${repository}"
        [ "${status}" = 200 ] || die "${OWNER}/${repository} has no release tagged ${tag} (HTTP ${status}); the pin ${f#"${REPO_ROOT}"/} names a commit that repository never published"
        local url digest
        url="$(jq -r --arg n "${asset}" '.assets[]? | select(.name == $n) | .url' "${work}/release.json" | head -n1)"
        digest="$(jq -r --arg n "${asset}" '.assets[]? | select(.name == $n) | .digest // empty' "${work}/release.json" | head -n1)"
        [ -n "${url}" ] || die "the release ${tag} of ${repository} carries no asset named ${asset}"
        if [ "${check}" = 1 ]; then
            [ -z "${digest}" ] || [ "${digest}" = "sha256:${sha}" ] || die "${name}: the release publishes ${asset} with ${digest}, and the pin says sha256:${sha}"
            echo "deps.sh: ${name} at ${commit:0:12} is published${digest:+ (digest matches the pin)}"
            continue
        fi
        status="$(download "${url}" "${work}/${asset}")"
        explain "${status}" "downloading ${asset}"
        [ "${status}" = 200 ] || die "downloading ${asset} answered HTTP ${status}"
        local got; got="$(sha256sum "${work}/${asset}" | cut -d' ' -f1)"
        [ "${got}" = "${sha}" ] || die "${name}: ${asset} hashes to ${got}, and the pin says ${sha}; the download was discarded"
        rm -rf "${work}/tree"; mkdir -p "${work}/tree"
        tar -xzf "${work}/${asset}" -C "${work}/tree" --strip-components=1
        [ -n "$(ls -A "${work}/tree")" ] || die "${asset} unpacked to nothing"
        rm -rf "${dest}"; mkdir -p "$(dirname "${dest}")"; mv "${work}/tree" "${dest}"
        printf '%s\n' "${sha}" >"${dest}/.deps-pin"
        echo "deps.sh: ${path}/ is now ${name} at ${commit:0:12} (${asset}, verified)"
    done
}

cmd_bump() {
    local repository="${1:-}" tag="" path=""
    [ -n "${repository}" ] || die "usage: bash tools/deps.sh bump <repository> [--tag build-<commit12>] [--path <dir>]"
    shift
    while [ "$#" -gt 0 ]; do
        case "$1" in
        --tag) tag="${2-}"; [ -n "${tag}" ] || die "--tag takes build-<commit12>"; shift 2 ;;
        --path) path="${2-}"; [ -n "${path}" ] || die "--path takes a directory"; shift 2 ;;
        *) die "unknown argument: $1" ;;
        esac
    done
    [[ "${repository}" =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ ]] || die "'${repository}' is not a repository name"
    local file="${PINS}/${repository}.json"
    if [ -z "${path}" ]; then
        [ -f "${file}" ] || die "${file#"${REPO_ROOT}"/} does not exist yet; say where the dependency lives with --path <dir>"
        path="$(jq -r '.path' "${file}")"
    fi
    token
    local work; work="$(mktemp -d)"; trap 'rm -rf "${work}"' RETURN
    local status
    if [ -z "${tag}" ]; then
        status="$(api GET "${API}/repos/${OWNER}/${repository}/releases?per_page=30" "${work}/releases.json")"
        explain "${status}" "listing the releases of ${repository}"
        [ "${status}" = 200 ] || die "${OWNER}/${repository} does not exist or is not readable (HTTP ${status})"
        tag="$(jq -r '[.[] | select(.draft == false and (.tag_name | startswith("build-")))][0].tag_name // empty' "${work}/releases.json")"
        [ -n "${tag}" ] || die "${OWNER}/${repository} has no build-<commit12> release"
    fi
    [[ "${tag}" =~ ^build-[0-9a-f]{12}$ ]] || die "the tag '${tag}' is not build-<commit12>"
    status="$(release "${repository}" "${tag}" "${work}/release.json")"
    explain "${status}" "reading the release ${tag}"
    [ "${status}" = 200 ] || die "${OWNER}/${repository} has no release tagged ${tag}"
    local commit asset url
    commit="$(jq -r '.target_commitish' "${work}/release.json")"
    asset="${repository}-${tag#build-}.tar.gz"
    url="$(jq -r --arg n "${asset}" '.assets[]? | select(.name == $n) | .url' "${work}/release.json" | head -n1)"
    [ -n "${url}" ] || die "the release ${tag} of ${repository} carries no source asset ${asset}"
    status="$(download "${url}" "${work}/${asset}")"
    [ "${status}" = 200 ] || die "downloading ${asset} answered HTTP ${status}"
    local sha; sha="$(sha256sum "${work}/${asset}" | cut -d' ' -f1)"
    # The archive's own top-level directory names the commit; that is what the
    # pin records, and target_commitish is only checked against it.
    # sed reads the whole listing (an early-exiting reader would SIGPIPE tar under pipefail).
    local top; top="$(tar -tzf "${work}/${asset}" | sed -n '1{s|/.*||;p}')"
    [ "${top}" = "${repository}-${tag#build-}" ] || die "${asset} unpacks to '${top}', not ${repository}-${tag#build-}"
    [[ "${commit}" =~ ^[0-9a-f]{40}$ ]] && [ "${commit:0:12}" = "${tag#build-}" ] || die "the release ${tag} points at '${commit}', which does not name the commit in its tag"
    mkdir -p "${PINS}"
    [ -f "${file}" ] && cp "${file}" "${work}/old.json" || printf '{}\n' >"${work}/old.json"
    jq -n --arg name "${repository}" --arg repository "${repository}" --arg commit "${commit}" --arg path "${path}" --arg asset "${asset}" --arg sha "${sha}" \
        '{name: $name, repository: $repository, commit: $commit, path: $path, asset: $asset, sha256: $sha}' >"${work}/new.json"
    if diff -u --label "a/${file#"${REPO_ROOT}"/}" --label "b/${file#"${REPO_ROOT}"/}" "${work}/old.json" "${work}/new.json"; then
        echo "deps.sh: ${file#"${REPO_ROOT}"/} already pins ${repository} at ${tag}; no change"
        return 0
    fi
    cp "${work}/new.json" "${file}"
    echo "deps.sh: ${file#"${REPO_ROOT}"/} rewritten for ${repository} at ${tag}; review the diff above, then \`make deps\`"
}

cmd_publish_source() {
    local repository
    if [ -n "${MICA_SOURCE_REPO:-}" ]; then repository="${MICA_SOURCE_REPO}"
    else
        local origin_url; origin_url="$(git -C "${REPO_ROOT}" remote get-url origin 2>/dev/null || true)"
        repository="$(basename "${origin_url%/}" .git)"
        [ -n "${origin_url}" ] && [ -n "${repository}" ] || die "${REPO_ROOT} has no origin remote; set MICA_SOURCE_REPO=<name>"
    fi
    [ -z "$(git -C "${REPO_ROOT}" status --porcelain)" ] || die "${REPO_ROOT} has uncommitted changes; a source release is one commit's tree"
    local commit tag asset; commit="$(git -C "${REPO_ROOT}" rev-parse HEAD)"; tag="build-${commit:0:12}"; asset="${repository}-${commit:0:12}.tar.gz"
    token
    local work; work="$(mktemp -d)"; trap 'rm -rf "${work}"' RETURN
    git -C "${REPO_ROOT}" archive --format=tar.gz --prefix="${repository}-${commit:0:12}/" -o "${work}/${asset}" HEAD
    local sha; sha="$(sha256sum "${work}/${asset}" | cut -d' ' -f1)"
    local status
    status="$(release "${repository}" "${tag}" "${work}/release.json")"
    explain "${status}" "reading the release ${tag}"
    if [ "${status}" = 404 ]; then
        jq -n --arg tag "${tag}" --arg commit "${commit}" --arg repo "${repository}" \
            '{tag_name: $tag, target_commitish: $commit, name: $tag, body: ("Built by " + $repo + " from " + $commit + "."), draft: false, prerelease: false}' >"${work}/create.json"
        status="$(api POST "${API}/repos/${OWNER}/${repository}/releases" "${work}/release.json" --data-binary "@${work}/create.json")"
        explain "${status}" "creating the release ${tag}"
        [ "${status}" = 201 ] || die "creating the release ${tag} answered HTTP ${status}: $(head -c 300 "${work}/release.json"). The commit must be pushed to ${OWNER}/${repository} first"
        echo "deps.sh: release ${tag} created on ${commit:0:12}"
    elif [ "${status}" != 200 ]; then die "reading the release ${tag} answered HTTP ${status}"; fi
    local id url; id="$(jq -r '.id' "${work}/release.json")"
    url="$(jq -r --arg n "${asset}" '.assets[]? | select(.name == $n) | .url' "${work}/release.json" | head -n1)"
    if [ -z "${url}" ]; then
        status="$(api POST "${UPLOAD}/repos/${OWNER}/${repository}/releases/${id}/assets?name=${asset}" "${work}/asset.json" -H 'Content-Type: application/gzip' --data-binary "@${work}/${asset}")"
        explain "${status}" "uploading ${asset}"
        [ "${status}" = 201 ] || die "uploading ${asset} answered HTTP ${status}: $(head -c 300 "${work}/asset.json")"
        url="$(jq -r '.url' "${work}/asset.json")"
        echo "deps.sh: ${asset} uploaded"
    else
        echo "deps.sh: ${asset} is already on ${tag}; comparing bytes"
    fi
    status="$(download "${url}" "${work}/back.tar.gz")"
    [ "${status}" = 200 ] || die "reading ${asset} back answered HTTP ${status}"
    local got; got="$(sha256sum "${work}/back.tar.gz" | cut -d' ' -f1)"
    [ "${got}" = "${sha}" ] || die "the release serves ${asset} with sha256 ${got}, and this tree's archive is ${sha}; under one name the release holds other bytes"
    echo "deps.sh: ${repository} at ${commit:0:12} is published as ${asset} (sha256 ${sha}); pin it in a consumer with: bash tools/deps.sh bump ${repository} --tag ${tag}"
}

case "${1:-}" in
fetch) shift; cmd_fetch "$@" ;;
bump) shift; cmd_bump "$@" ;;
publish-source) shift; cmd_publish_source "$@" ;;
*) echo "usage: bash tools/deps.sh fetch [--check] | bump <repository> [--tag build-<commit12>] [--path <dir>] | publish-source" >&2; exit 1 ;;
esac

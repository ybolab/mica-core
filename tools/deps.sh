#!/usr/bin/env bash
# Source dependencies by pin: fetch, check, bump and publish.
#
#   bash tools/deps.sh fetch [--check]              every deps/sources/*.json into its path
#   bash tools/deps.sh bump <repository> [--tag build-<commit12>] [--path <dir>]
#   bash tools/deps.sh publish-source [--revision <40 hex> --expect-sha256 <64 hex>]
#                                                   HEAD, or one commit of its history, as a source artifact
#
# A source dependency is a pin, one JSON file per repository under
# deps/sources/, in the shape of the Debian pins under rootfs/debian/packages:
#
#   { "name": "mica-build-env", "repository": "mica-build-env",
#     "commit": "<40 hex>", "path": "build-env",
#     "asset": "mica-build-env-<commit12>.tar.gz", "sha256": "<64 hex>" }
#
# The asset is the `git archive` of that commit, the one layer of the OCI
# artifact <registry>/<repository>:source.build-<commit12>, in the package of
# the repository itself, pushed by its own CI (`publish-source`); its sha256
# is the layer's digest. A pin that predates per-repository packages names a
# blob of <registry>/mica-source (tagged <repository>.build-<commit12>), and
# fetch and bump still find it there, asked only after the repository's own
# package answered 404 and checked the same way.
#
# The archive is `git -c tar.tar.gz.command='gzip -cn' archive
# --format=tar.gz --prefix=<repository>-<commit12>/ <commit>`: git 2.38 and
# later compress tar.gz in-process with other bytes, so the compressor is
# fixed or one commit would have two digests. `publish-source --revision
# <commit> --expect-sha256 <sha>` publishes a commit of HEAD's history --
# its tree, date and revision only, nothing of the publishing checkout -- and
# refuses before any registry access unless the archive is byte-identical to
# the digest consumers already pin. `fetch` reads the blob by that
# digest, verifies it, replaces <path> with its contents and records the pin
# in <path>/.deps-pin, so a second fetch is a no-op and the lineage record
# can require the checkout to match the pin. <path> is gitignored: what is
# there is the dependency at the pin, never edited in place -- a change is a
# commit in the dependency, a publish, and a bump here.
#
# This file is the same in every Mica OS repository, VENDORED rather than
# fetched, because it is what fetches everything else: bash, curl, jq, tar
# and sha256sum only, and the OCI client below is build-env/deb/oci.sh
# verbatim for the same reason. The registry, user and token variable are
# the defaults below; MICA_REGISTRY, MICA_REGISTRY_USER, MICA_REGISTRY_PLAIN_HTTP
# and MICA_DEPS_TOKEN_VAR override them (the tests drive a local registry
# that way), and the token falls back to `gh auth token`. Nothing prints the token.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${HERE}/.." && pwd)"
MICA_REGISTRY="${MICA_REGISTRY:-ghcr.io/ybolab}"
MICA_REGISTRY_USER="${MICA_REGISTRY_USER:-ybolab}"
TOKEN_VAR="${MICA_DEPS_TOKEN_VAR:-GH_TOKEN}"
MICA_RELEASE_TOKEN_VAR="${TOKEN_VAR}"
PINS="${MICA_DEPS_DIR:-${REPO_ROOT}/deps/sources}"

die() { echo "deps.sh: error: $*" >&2; exit 1; }
for t in curl jq tar sha256sum git; do
    command -v "${t}" >/dev/null 2>&1 || die "${t} is required and not on PATH"
done

# ---- the OCI client (build-env/deb/oci.sh, verbatim) ----
OCI_EMPTY_CONFIG_DIGEST=sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a
OCI_MANIFEST_TYPE=application/vnd.oci.image.manifest.v1+json

oci_load() { # from MICA_REGISTRY (and MICA_REGISTRY_PLAIN_HTTP=1 for a test registry)
    [[ "${MICA_REGISTRY}" =~ ^([A-Za-z0-9.-]+(:[0-9]+)?)/([A-Za-z0-9][A-Za-z0-9._/-]*[A-Za-z0-9])$ ]] || {
        echo "error: MICA_REGISTRY='${MICA_REGISTRY}' is not <host>[:port]/<owner>" >&2; return 1; }
    OCI_HOST="${BASH_REMATCH[1]}"
    OCI_BASE="${BASH_REMATCH[3]}"
    if [ "${MICA_REGISTRY_PLAIN_HTTP:-0}" = 1 ]; then
        case "${OCI_HOST}" in 127.0.0.1* | localhost* | *.local | *:*) ;; *) [[ "${OCI_HOST}" =~ ^[a-z0-9-]+(:[0-9]+)?$ ]] || { echo "error: MICA_REGISTRY_PLAIN_HTTP=1 is for a local test registry, not ${OCI_HOST}" >&2; return 1; } ;; esac
        OCI_URL="http://${OCI_HOST}"
    else
        OCI_URL="https://${OCI_HOST}"
    fi
}

oci_repo() { # <repository>
    [[ "$1" =~ ^[a-z0-9][a-z0-9-]*$ ]] || { echo "error: '$1' is not a repository name a package can carry ([a-z0-9-])" >&2; return 1; }
    printf '%s/%s\n' "${OCI_BASE}" "$1"
}

oci_legacy_repo() { # <kind>
    printf '%s/mica-%s\n' "${OCI_BASE}" "$1"
}

oci_tag() { # <name>... <build-tag>
    local IFS=.
    printf '%s\n' "$*"
}

# The newest build-<commit12> published under <prefix>, by the created
# annotation of each manifest (a tag carries no date); empty when none.
oci_newest() { # <repo> <prefix>
    local repo="$1" prefix="$2" tag out best="" best_created="" created
    out="$(mktemp)"
    while IFS= read -r tag; do
        case "${tag}" in "${prefix}".build-*) ;; *) continue ;; esac
        [[ "${tag#"${prefix}".}" =~ ^build-[0-9a-f]{12}$ ]] || continue
        [ "$(oci_manifest_get "${repo}" "${tag}" "${out}")" = 200 ] || continue
        created="$(jq -r '.annotations["org.opencontainers.image.created"] // empty' "${out}")"
        [ -n "${created}" ] || continue
        if [ -z "${best}" ] || [[ "${created}" > "${best_created}" ]]; then best="${tag#"${prefix}".}"; best_created="${created}"; fi
    done < <(oci_tags "${repo}")
    rm -f "${out}"
    printf '%s' "${best}"
}

# Whether <repo>:<ref> is readable with no credential at all: the anonymous
# token the realm issues, then the manifest. Every publisher runs it after a
# push, so an artifact that went out private is a red job naming the package
# to make public, not a consumer's 401 a week later.
oci_public() { # <repo> <ref>
    local repo="$1" ref="$2" challenge realm service t auth=()
    challenge="$(curl -sS --max-time 60 -o /dev/null -D - "${OCI_URL}/v2/${repo}/tags/list" 2>/dev/null | tr -d '\r' | grep -i '^www-authenticate: bearer' || true)"
    if [ -n "${challenge}" ]; then
        realm="$(printf '%s' "${challenge}" | sed -n 's/.*realm="\([^"]*\)".*/\1/p')"
        service="$(printf '%s' "${challenge}" | sed -n 's/.*service="\([^"]*\)".*/\1/p')"
        t="$(curl -sS --max-time 60 --get --data-urlencode "service=${service}" --data-urlencode "scope=repository:${repo}:pull" "${realm}" 2>/dev/null | jq -r '.token // .access_token // empty' 2>/dev/null || true)"
        [ -z "${t}" ] || auth=(-H "Authorization: Bearer ${t}")
    fi
    [ "$(curl -sS --max-time 60 -o /dev/null -w '%{http_code}' "${auth[@]}" -H "Accept: ${OCI_MANIFEST_TYPE}" "${OCI_URL}/v2/${repo}/manifests/${ref}" 2>/dev/null || echo 000)" = 200 ]
}

# The refusal a publisher prints when its artifact is not public.
oci_require_public() { # <repo> <ref>
    oci_public "$1" "$2" && return 0
    echo "error: ${OCI_HOST}/$1:$2 was published and cannot be pulled anonymously: the package $1 is private. Every Mica OS package is public; set it once at https://github.com/orgs/${OCI_BASE}/packages/container/package/${1#"${OCI_BASE}"/} (Package settings, Danger Zone, Change visibility: Public) and rerun -- every later artifact of this repository is public from then on" >&2
    return 1
}

# A bearer for <repo> with <actions> (pull | pull,push), from the challenge
# the registry gives an unauthenticated request: "200 <bearer>" (the bearer
# empty when the registry does not challenge), or the token endpoint's own
# status and no bearer -- 401/403 when it refuses, 000 when it cannot be
# reached -- so a refusal reaches the caller as what it is.
oci_bearer() {
    local repo="$1" actions="$2" challenge realm service out code token cred=()
    challenge="$(curl -sS --max-time 60 -o /dev/null -D - "${OCI_URL}/v2/${repo}/tags/list" 2>/dev/null | tr -d '\r' | grep -i '^www-authenticate: bearer' || true)"
    [ -n "${challenge}" ] || { printf '200 \n'; return 0; }
    realm="$(printf '%s' "${challenge}" | sed -n 's/.*realm="\([^"]*\)".*/\1/p')"
    service="$(printf '%s' "${challenge}" | sed -n 's/.*service="\([^"]*\)".*/\1/p')"
    [ -n "${realm}" ] || { echo "error: ${OCI_HOST} challenged with no realm: ${challenge}" >&2; printf '000 \n'; return 0; }
    # With a token, as that identity; without one, anonymously -- a public
    # artifact is read that way, and a private one is refused here.
    [ -z "${REGISTRY_TOKEN:-}" ] || cred=(-u "${MICA_REGISTRY_USER}:${REGISTRY_TOKEN}")
    out="$(mktemp)"
    code="$(curl -sS --max-time 60 -o "${out}" -w '%{http_code}' "${cred[@]}" \
        --get --data-urlencode "service=${service}" --data-urlencode "scope=repository:${repo}:${actions}" "${realm}" 2>/dev/null || echo 000)"
    token="$(jq -r '.token // .access_token // empty' "${out}" 2>/dev/null || true)"
    rm -f "${out}"
    if [ "${code}" = 200 ] && [ -n "${token}" ]; then printf '200 %s\n' "${token}"; return 0; fi
    [ "${code}" != 200 ] || code=000
    echo "error: ${realm} answered ${code} for repository:${repo}:${actions}, issuing no token${REGISTRY_TOKEN:+; ${MICA_RELEASE_TOKEN_VAR} does not grant it}${REGISTRY_TOKEN:- (anonymously: the package is private or does not exist)}" >&2
    printf '%s \n' "${code}"
}

# <method> <repo> <actions> <path-under-v2/repo> <out> [curl args] -> status
oci_request() {
    local method="$1" repo="$2" actions="$3" path="$4" out="$5"; shift 5
    local line bearer auth=()
    line="$(oci_bearer "${repo}" "${actions}")"
    [ "${line%% *}" = 200 ] || { printf '%s' "${line%% *}"; return 0; }
    bearer="${line#* }"
    [ -z "${bearer}" ] || auth=(-H "Authorization: Bearer ${bearer}")
    curl -sS --max-time 1800 -o "${out}" -w '%{http_code}' -X "${method}" "${auth[@]}" "$@" "${OCI_URL}/v2/${repo}/${path}" 2>/dev/null || echo 000
}

oci_manifest_get() { # <repo> <tag|digest> <out> -> status
    oci_request GET "$1" pull "manifests/$2" "$3" -H "Accept: ${OCI_MANIFEST_TYPE}"
}
oci_manifest_digest() { printf 'sha256:%s' "$(sha256sum "$1" | cut -d' ' -f1)"; }
oci_blob_get() { # <repo> <digest> <out> -> status; the storage redirect is followed
    oci_request GET "$1" pull "blobs/$2" "$3" -L
}
oci_blob_head() { # <repo> <digest> -> status
    oci_request HEAD "$1" pull "blobs/$2" /dev/null -I
}
# Which of <repo>... holds <digest>, in order: "<status> TAB <repo>". The
# next repository is asked only after an actual 404; any other answer (401,
# 403, a transport failure) stops the search and is returned with no
# repository, so a refusal is never papered over by another package.
oci_blob_where() { # <digest> <repo>...
    local digest="$1" repo status=404
    shift
    for repo in "$@"; do
        status="$(oci_blob_head "${repo}" "${digest}")" || status=000
        [ "${status}" != 200 ] || { printf '200\t%s\n' "${repo}"; return 0; }
        [ "${status}" = 404 ] || break
    done
    printf '%s\t\n' "${status}"
}
oci_tags() { # <repo>: every tag, one per line; empty (status 404) for a repository nobody pushed
    local repo="$1" out status last="" page
    out="$(mktemp)"
    while :; do
        status="$(oci_request GET "${repo}" pull "tags/list?n=100${last:+&last=${last}}" "${out}")"
        case "${status}" in
        200) ;;
        404) rm -f "${out}"; return 0 ;;
        *) rm -f "${out}"; echo "error: listing the tags of ${OCI_HOST}/${repo} answered HTTP ${status}" >&2; return 1 ;;
        esac
        page="$(jq -r '.tags[]?' "${out}")"
        [ -n "${page}" ] || break
        printf '%s\n' "${page}"
        last="$(printf '%s\n' "${page}" | tail -n1)"
        [ "$(printf '%s\n' "${page}" | wc -l)" -ge 100 ] || break
    done
    rm -f "${out}"
}

# <repo> <file> <digest>: upload the blob unless the registry has it.
oci_blob_put() {
    local repo="$1" file="$2" digest="$3" status out location
    status="$(oci_blob_head "${repo}" "${digest}")"
    [ "${status}" != 200 ] || return 0
    out="$(mktemp)"
    status="$(oci_request POST "${repo}" pull,push "blobs/uploads/" "${out}" -D "${out}.h" -H 'Content-Length: 0')"
    [ "${status}" = 202 ] || { echo "error: starting an upload to ${OCI_HOST}/${repo} answered HTTP ${status}: $(head -c 200 "${out}")" >&2; rm -f "${out}" "${out}.h"; return 1; }
    location="$(tr -d '\r' <"${out}.h" | sed -n 's/^[Ll]ocation: //p' | head -n1)"
    rm -f "${out}.h"
    [ -n "${location}" ] || { echo "error: the upload to ${OCI_HOST}/${repo} came with no Location" >&2; rm -f "${out}"; return 1; }
    case "${location}" in /*) location="${OCI_URL}${location}" ;; esac
    case "${location}" in *\?*) location="${location}&digest=${digest}" ;; *) location="${location}?digest=${digest}" ;; esac
    local line bearer auth=()
    line="$(oci_bearer "${repo}" pull,push)"
    [ "${line%% *}" = 200 ] || { echo "error: uploading ${digest} to ${OCI_HOST}/${repo}: the token endpoint answered ${line%% *}" >&2; rm -f "${out}"; return 1; }
    bearer="${line#* }"
    [ -z "${bearer}" ] || auth=(-H "Authorization: Bearer ${bearer}")
    status="$(curl -sS --max-time 1800 -o "${out}" -w '%{http_code}' -X PUT "${auth[@]}" -H 'Content-Type: application/octet-stream' --data-binary "@${file}" "${location}" 2>/dev/null || echo 000)"
    [ "${status}" = 201 ] || { echo "error: uploading ${digest} to ${OCI_HOST}/${repo} answered HTTP ${status}: $(head -c 200 "${out}")" >&2; rm -f "${out}"; return 1; }
    rm -f "${out}"
}

# <repo> <tag> <artifact-type> <annotations.json> <layers.tsv> -> prints the manifest digest.
oci_push() {
    local repo="$1" tag="$2" type="$3" annotations="$4" layers="$5"
    local work manifest file media title digest size status
    work="$(mktemp -d)"
    printf '{}' >"${work}/config"
    oci_blob_put "${repo}" "${work}/config" "${OCI_EMPTY_CONFIG_DIGEST}" || { rm -rf "${work}"; return 1; }
    : >"${work}/layers.json"
    while IFS=$'\t' read -r file media title; do
        [ -n "${file}" ] || continue
        digest="sha256:$(sha256sum "${file}" | cut -d' ' -f1)"
        size="$(stat -c %s "${file}")"
        oci_blob_put "${repo}" "${file}" "${digest}" || { rm -rf "${work}"; return 1; }
        jq -n --arg m "${media}" --arg d "${digest}" --argjson s "${size}" --arg t "${title}" \
            '{mediaType: $m, digest: $d, size: $s, annotations: {"org.opencontainers.image.title": $t}}' >>"${work}/layers.json"
    done <"${layers}"
    jq -n --arg type "${type}" --arg cfg "${OCI_EMPTY_CONFIG_DIGEST}" --slurpfile layers "${work}/layers.json" --slurpfile ann "${annotations}" \
        '{schemaVersion: 2, mediaType: "application/vnd.oci.image.manifest.v1+json", artifactType: $type,
          config: {mediaType: "application/vnd.oci.empty.v1+json", digest: $cfg, size: 2},
          layers: $layers, annotations: $ann[0]}' >"${work}/manifest.json"
    manifest="${work}/manifest.json"
    status="$(oci_request PUT "${repo}" pull,push "manifests/${tag}" "${work}/put.out" -H "Content-Type: ${OCI_MANIFEST_TYPE}" --data-binary "@${manifest}")"
    [ "${status}" = 201 ] || { echo "error: putting the manifest ${tag} to ${OCI_HOST}/${repo} answered HTTP ${status}: $(head -c 200 "${work}/put.out")" >&2; rm -rf "${work}"; return 1; }
    oci_manifest_digest "${manifest}"
    rm -rf "${work}"
}

# ---- end of the OCI client ----

oci_load || exit 1
# The token, else none: the source artifacts are public and a fetch needs
# no token (an anonymous pull token is asked for); publish-source does.
token() {
    REGISTRY_TOKEN="${!TOKEN_VAR:-}"
    if [ -z "${REGISTRY_TOKEN}" ] && [ -z "${MICA_DEPS_NO_GH:-}" ] && command -v gh >/dev/null 2>&1; then
        REGISTRY_TOKEN="$(gh auth token 2>/dev/null || true)"
    fi
    [ "${1:-}" != --write ] || [ -n "${REGISTRY_TOKEN}" ] || die "${TOKEN_VAR} is unset or empty and \`gh auth token\` gave nothing. Publishing to ${OCI_HOST} needs a token with write:packages in ${TOKEN_VAR}; publishing is CI's, whose own token has it"
}
explain() { # status what
    case "$1" in
    401 | 403) die "$2 answered $1; ${TOKEN_VAR} does not grant access to ${OCI_HOST}" ;;
    000) die "$2: ${OCI_HOST} could not be reached (transport failure)" ;;
    esac
}
# One repository's source at one commit: <owner>/<repository>:source.build-<commit12>,
# and where it was before per-repository packages: <owner>/mica-source:<repository>.build-<commit12>.
source_repo() { oci_repo "$1"; } # <repository>
source_ref() { oci_tag source "$1"; } # <build-tag>
legacy_source_repo() { oci_legacy_repo source; }
legacy_source_ref() { oci_tag "$1" "$2"; } # <repository> <build-tag>
# The one archive of a commit: fixed compressor, fixed top-level directory.
source_archive() { # <repository> <commit> <out>
    git -C "${REPO_ROOT}" -c tar.tar.gz.command='gzip -cn' archive --format=tar.gz --prefix="$1-${2:0:12}/" -o "$3" "$2"
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
        local dest="${REPO_ROOT}/${path}" tag="build-${commit:0:12}" repo status own
        own="$(source_repo "${repository}")" || exit 1
        if [ "${check}" = 0 ] && [ -f "${dest}/.deps-pin" ] && [ "$(cat "${dest}/.deps-pin")" = "${sha}" ]; then
            echo "deps.sh: ${path}/ is ${name} at ${commit:0:12} already"
            continue
        fi
        # The repository's own package, else the shared one the pin predates.
        IFS=$'\t' read -r status repo < <(oci_blob_where "sha256:${sha}" "${own}" "$(legacy_source_repo)")
        explain "${status}" "reading ${OCI_HOST}/${own}"
        [ "${status}" = 200 ] || die "${OCI_HOST}/${own} holds no blob sha256:${sha} (HTTP ${status}), nor does ${OCI_HOST}/$(legacy_source_repo); the pin ${f#"${REPO_ROOT}"/} names a commit ${repository} never published as $(source_ref "${tag}")"
        if [ "${check}" = 1 ]; then
            echo "deps.sh: ${name} at ${commit:0:12} is published (${OCI_HOST}/${repo}@sha256:${sha:0:12})"
            continue
        fi
        status="$(oci_blob_get "${repo}" "sha256:${sha}" "${work}/${asset}")"
        explain "${status}" "downloading ${asset}"
        [ "${status}" = 200 ] || die "downloading ${asset} from ${OCI_HOST}/${repo} answered HTTP ${status}"
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
    local repo status ref own; own="$(source_repo "${repository}")" || exit 1
    # The repository's own package; else, for a repository that has not
    # published there yet, the shared package of before.
    repo="${own}"
    if [ -z "${tag}" ]; then
        tag="$(oci_newest "${own}" source)" || exit 1
        if [ -z "${tag}" ]; then
            repo="$(legacy_source_repo)"
            tag="$(oci_newest "${repo}" "${repository}")" || exit 1
        fi
        [ -n "${tag}" ] || die "${OCI_HOST}/${own} has no source.build-<commit12> artifact (nor ${OCI_HOST}/$(legacy_source_repo) a ${repository}.build-<commit12>)"
    fi
    [[ "${tag}" =~ ^build-[0-9a-f]{12}$ ]] || die "the tag '${tag}' is not build-<commit12>"
    ref="$(source_ref "${tag}")"
    [ "${repo}" = "${own}" ] || ref="$(legacy_source_ref "${repository}" "${tag}")"
    status="$(oci_manifest_get "${repo}" "${ref}" "${work}/manifest.json")"
    if [ "${status}" = 404 ] && [ "${repo}" = "${own}" ]; then
        repo="$(legacy_source_repo)"; ref="$(legacy_source_ref "${repository}" "${tag}")"
        status="$(oci_manifest_get "${repo}" "${ref}" "${work}/manifest.json")"
    fi
    explain "${status}" "reading ${OCI_HOST}/${repo}:${ref}"
    [ "${status}" = 200 ] || die "${OCI_HOST}/${own} has no artifact $(source_ref "${tag}") (HTTP ${status}), nor ${OCI_HOST}/$(legacy_source_repo) a $(legacy_source_ref "${repository}" "${tag}")"
    local commit asset digest
    [ "$(jq -r '.artifactType // empty' "${work}/manifest.json")" = application/vnd.mica.source ] || die "${OCI_HOST}/${repo}:${ref} is a '$(jq -r '.artifactType // "(none)"' "${work}/manifest.json")' artifact, not application/vnd.mica.source"
    [ "$(jq -r '.annotations["mica.source-repo"] // empty' "${work}/manifest.json")" = "${repository}" ] || die "${OCI_HOST}/${repo}:${ref} says mica.source-repo='$(jq -r '.annotations["mica.source-repo"] // ""' "${work}/manifest.json")', not ${repository}; a package holds only its own repository's artifacts"
    commit="$(jq -r '.annotations["org.opencontainers.image.revision"] // empty' "${work}/manifest.json")"
    [ "$(jq -r '.layers | length' "${work}/manifest.json")" = 1 ] && [ "$(jq -r '.layers[0].mediaType' "${work}/manifest.json")" = application/vnd.mica.source.tar+gzip ] || die "${OCI_HOST}/${repo}:${ref} carries $(jq -r '.layers | length' "${work}/manifest.json") layer(s) of $(jq -r '[.layers[].mediaType] | unique | join(",")' "${work}/manifest.json"); a source artifact is one application/vnd.mica.source.tar+gzip"
    asset="$(jq -r '.layers[0].annotations["org.opencontainers.image.title"] // empty' "${work}/manifest.json")"
    digest="$(jq -r '.layers[0].digest' "${work}/manifest.json")"
    [ "${asset}" = "${repository}-${tag#build-}.tar.gz" ] || die "${OCI_HOST}/${repo}:${ref} carries '${asset}', not ${repository}-${tag#build-}.tar.gz"
    [[ "${commit}" =~ ^[0-9a-f]{40}$ ]] && [ "${commit:0:12}" = "${tag#build-}" ] || die "the artifact ${tag} says it was built from '${commit}', which does not name the commit in its tag"
    status="$(oci_blob_get "${repo}" "${digest}" "${work}/${asset}")"
    [ "${status}" = 200 ] || die "downloading ${asset} answered HTTP ${status}"
    local sha; sha="$(sha256sum "${work}/${asset}" | cut -d' ' -f1)"
    [ "${digest}" = "sha256:${sha}" ] || die "${asset} hashes to ${sha} and the manifest says ${digest}"
    # The archive's own top-level directory names the commit; that is what the
    # pin records. sed reads the whole listing (an early-exiting reader would SIGPIPE tar under pipefail).
    local top; top="$(tar -tzf "${work}/${asset}" | sed -n '1{s|/.*||;p}')"
    [ "${top}" = "${repository}-${tag#build-}" ] || die "${asset} unpacks to '${top}', not ${repository}-${tag#build-}"
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
    local revision="" expect=""
    while [ "$#" -gt 0 ]; do
        case "$1" in
        --revision) revision="${2-}"; shift 2 || die "--revision takes a full 40-hex commit id" ;;
        --expect-sha256) expect="${2-}"; shift 2 || die "--expect-sha256 takes 64 lowercase hex digits" ;;
        *) die "usage: bash tools/deps.sh publish-source [--revision <40 hex> --expect-sha256 <64 hex>]" ;;
        esac
    done
    # Every refusal below comes before any registry access or token.
    [ -z "${revision}" ] || [[ "${revision}" =~ ^[0-9a-f]{40}$ ]] || die "--revision takes a full 40-hex commit id, not '${revision}'; resolve a branch or tag to the approved commit first"
    [ -z "${expect}" ] || [[ "${expect}" =~ ^[0-9a-f]{64}$ ]] || die "--expect-sha256 takes 64 lowercase hex digits, not '${expect}'"
    [ -z "${revision}" ] || [ -n "${expect}" ] || die "--expect-sha256 is required with --revision: a historical publication must match the bytes consumers already pin"
    command -v gzip >/dev/null 2>&1 || die "gzip is required: the archive is compressed by gzip -cn"
    local repository
    if [ -n "${MICA_SOURCE_REPO:-}" ]; then repository="${MICA_SOURCE_REPO}"
    else
        local origin_url; origin_url="$(git -C "${REPO_ROOT}" remote get-url origin 2>/dev/null || true)"
        repository="$(basename "${origin_url%/}" .git)"
        [ -n "${origin_url}" ] && [ -n "${repository}" ] || die "${REPO_ROOT} has no origin remote; set MICA_SOURCE_REPO=<name>"
    fi
    local commit
    if [ -z "${revision}" ]; then
        [ -z "$(git -C "${REPO_ROOT}" status --porcelain)" ] || die "${REPO_ROOT} has uncommitted changes; a source artifact is one commit's tree"
        commit="$(git -C "${REPO_ROOT}" rev-parse HEAD)"
    else
        [ "$(git -C "${REPO_ROOT}" cat-file -t "${revision}" 2>/dev/null || true)" = commit ] || die "${revision} is not a commit of ${REPO_ROOT} (a shallow checkout lacks history; fetch it whole)"
        git -C "${REPO_ROOT}" merge-base --is-ancestor "${revision}" HEAD || die "${revision} is not in the history of HEAD; only this repository's own commits are published"
        commit="${revision}"
    fi
    local tag asset created repo ref
    tag="build-${commit:0:12}"; asset="${repository}-${commit:0:12}.tar.gz"
    created="$(git -C "${REPO_ROOT}" show -s --format=%cI "${commit}")"
    repo="$(source_repo "${repository}")" || exit 1; ref="$(source_ref "${tag}")"
    local work; work="$(mktemp -d)"; trap 'rm -rf "${work}"' RETURN
    source_archive "${repository}" "${commit}" "${work}/${asset}"
    local sha; sha="$(sha256sum "${work}/${asset}" | cut -d' ' -f1)"
    [ -z "${expect}" ] || [ "${sha}" = "${expect}" ] || die "the archive of ${commit} hashes to sha256 ${sha}, not the expected ${expect}; nothing was published"
    token --write
    local status
    status="$(oci_manifest_get "${repo}" "${ref}" "${work}/existing.json")"
    explain "${status}" "reading ${OCI_HOST}/${repo}:${ref}"
    if [ "${status}" = 200 ]; then
        local have; have="$(jq -r '.layers[0].digest // empty' "${work}/existing.json")"
        [ "${have}" = "sha256:${sha}" ] || die "${OCI_HOST}/${repo}:${ref} exists with ${have:-no layer}, and this commit's archive is sha256:${sha}; under one name the registry holds other bytes"
        [ "$(jq -r '[.artifactType, .annotations["org.opencontainers.image.revision"], .annotations["mica.source-repo"], (.layers | length), .layers[0].annotations["org.opencontainers.image.title"]] | map(tostring) | join(" ")' "${work}/existing.json")" = "application/vnd.mica.source ${commit} ${repository} 1 ${asset}" ] ||
            die "${OCI_HOST}/${repo}:${ref} holds this commit's bytes under another identity ($(jq -c '{artifactType, revision: .annotations["org.opencontainers.image.revision"], repo: .annotations["mica.source-repo"], layers: (.layers | length)}' "${work}/existing.json")); it is not re-pointed"
        echo "deps.sh: ${asset} is already ${OCI_HOST}/${repo}:${ref}; comparing bytes"
    elif [ "${status}" = 404 ]; then
        jq -n --arg repo "${repository}" --arg commit "${commit}" --arg created "${created}" \
            --arg url "https://github.com/${OCI_BASE}/${repository}" \
            '{"org.opencontainers.image.revision": $commit, "org.opencontainers.image.created": $created, "org.opencontainers.image.source": $url, "mica.source-repo": $repo, "mica.source-commit": $commit}' >"${work}/annotations.json"
        printf '%s\t%s\t%s\n' "${work}/${asset}" application/vnd.mica.source.tar+gzip "${asset}" >"${work}/layers.tsv"
        local digest; digest="$(oci_push "${repo}" "${ref}" application/vnd.mica.source "${work}/annotations.json" "${work}/layers.tsv")" || exit 1
        echo "deps.sh: ${asset} pushed as ${OCI_HOST}/${repo}:${ref} (${digest})"
    else die "reading ${OCI_HOST}/${repo}:${ref} answered HTTP ${status}"; fi
    oci_require_public "${repo}" "${ref}" || exit 1
    status="$(oci_blob_get "${repo}" "sha256:${sha}" "${work}/back.tar.gz")"
    [ "${status}" = 200 ] || die "reading ${asset} back answered HTTP ${status}"
    local got; got="$(sha256sum "${work}/back.tar.gz" | cut -d' ' -f1)"
    [ "${got}" = "${sha}" ] || die "the registry serves ${asset} with sha256 ${got}, and this commit's archive is ${sha}"
    echo "deps.sh: ${repository} at ${commit:0:12} is published as ${OCI_HOST}/${repo}:${ref} (sha256 ${sha}); pin it in a consumer with: bash tools/deps.sh bump ${repository} --tag ${tag}"
}

case "${1:-}" in
fetch) shift; cmd_fetch "$@" ;;
bump) shift; cmd_bump "$@" ;;
publish-source) shift; cmd_publish_source "$@" ;;
*) echo "usage: bash tools/deps.sh fetch [--check] | bump <repository> [--tag build-<commit12>] [--path <dir>] | publish-source [--revision <40 hex> --expect-sha256 <64 hex>]" >&2; exit 1 ;;
esac

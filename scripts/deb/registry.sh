#!/usr/bin/env bash
# Shared by fetch.sh, lock.sh, publish.sh and source.sh: the registry
# declaration, the token, the OCI client (oci.sh), the lock file and the
# origin-derived repository name. Sourced, not executed.
[ -n "${BASH_VERSION:-}" ] || { echo "registry.sh: bash only" >&2; exit 1; }

REGISTRY_HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REGISTRY_REPO_ROOT="$(cd "${REGISTRY_HERE}/../.." && pwd)"
# Overridable so tests can drive a stub API.
REGISTRY_ENV="${MICA_REGISTRY_ENV:-${REGISTRY_HERE}/registry.env}"
LOCK_DIR="${MICA_LOCK_DIR:-${REGISTRY_REPO_ROOT}/deps/packages}"

# shellcheck disable=SC1091
. "${REGISTRY_HERE}/oci.sh"

# registry.env is checked to be plain KEY=value before it is sourced.
registry_load() {
    [ -f "${REGISTRY_ENV}" ] || { echo "error: ${REGISTRY_ENV} does not exist; it declares where artifacts are published" >&2; return 1; }
    while IFS= read -r line; do
        case "${line}" in
        '' | '#'*) continue ;;
        *'$('* | *'`'*) echo "error: ${REGISTRY_ENV} carries a command substitution: ${line}" >&2; return 1 ;;
        esac
        [[ "${line}" =~ ^[A-Z][A-Z0-9_]*= ]] || { echo "error: ${REGISTRY_ENV} carries a line that is neither KEY=value nor a comment: ${line}" >&2; return 1; }
    done <"${REGISTRY_ENV}"
    MICA_REGISTRY=""
    MICA_REGISTRY_USER=""
    MICA_RELEASE_TOKEN_VAR=""
    MICA_SOURCE_URL=""
    # shellcheck disable=SC1090
    . "${REGISTRY_ENV}"
    for v in MICA_REGISTRY MICA_REGISTRY_USER MICA_RELEASE_TOKEN_VAR MICA_SOURCE_URL; do
        [ -n "${!v}" ] || { echo "error: ${REGISTRY_ENV} declares no ${v}" >&2; return 1; }
    done
    oci_load || return 1
    command -v jq >/dev/null 2>&1 || { echo "error: jq is required to read the registry and not on PATH" >&2; return 1; }
}

# The token, from the variable registry.env names, else `gh auth token`,
# else none: the artifacts are public and a read needs no token at all
# (the client asks the registry for an anonymous pull token). A write does;
# `registry_token --write` refuses without one. Never printed.
registry_token() {
    REGISTRY_TOKEN="${!MICA_RELEASE_TOKEN_VAR:-}"
    if [ -z "${REGISTRY_TOKEN}" ] && [ -z "${MICA_RELEASE_NO_GH:-}" ] && command -v gh >/dev/null 2>&1; then
        REGISTRY_TOKEN="$(gh auth token 2>/dev/null || true)"
    fi
    [ "${1:-}" != --write ] || [ -n "${REGISTRY_TOKEN}" ] || {
        echo "error: ${MICA_RELEASE_TOKEN_VAR} is unset or empty and \`gh auth token\` gave nothing. Publishing to ${OCI_HOST} needs a token with write:packages in that variable (scripts/deb/registry.env names it); publishing is CI's, whose own token has it" >&2
        return 1
    }
}

# The tag for a source commit: one artifact per commit, per repository and kind.
release_tag() { echo "build-${1:0:12}"; }

# One repository's pool for one architecture at one commit:
# <owner>/<repo>:pool.<arch>.build-<commit12>. Before per-repository packages
# it was <owner>/mica-pool:<repo>.<arch>.build-<commit12>; readers still look
# there (legacy_pool_*) for pins that predate the move.
pool_repo() { oci_repo "$1"; } # <repo>
pool_tag() { oci_tag pool "$1" "$2"; } # <arch> <build-tag>
legacy_pool_repo() { oci_legacy_repo pool; }
legacy_pool_tag() { oci_tag "$1" "$2" "$3"; } # <repo> <arch> <build-tag>

# Where <repo>'s blob <digest> is: its package, else the legacy pool package.
# "<status> TAB <artifact>" as oci_blob_where prints it.
pool_blob_where() { # <repo> <digest>
    local repo
    repo="$(pool_repo "$1")" || return 1
    oci_blob_where "$2" "${repo}" "$(legacy_pool_repo)"
}

# The annotations every artifact carries: the commit it was built from, when
# that commit was made (the newest artifact is the one with the latest
# created date; tags carry no date), and which repository built it.
artifact_annotations() { # <repo-name> <commit> <created-iso> <out.json>
    jq -n --arg repo "$1" --arg commit "$2" --arg created "$3" \
        --arg url "${MICA_SOURCE_URL%/}/$1" \
        '{"org.opencontainers.image.revision": $commit, "org.opencontainers.image.created": $created, "org.opencontainers.image.source": $url, "mica.source-repo": $repo, "mica.source-commit": $commit}' >"$4"
}

# The newest build-<commit12> of <repo> under <prefix> (oci.sh's oci_newest).
newest_tag() { oci_newest "$1" "$2"; } # <repo> <prefix>

# The repository this checkout is: MICA_SOURCE_REPO, else the basename of
# origin -- the same rule build.sh writes into Mica-Source-Repo.
registry_repo_name() {
    if [ -n "${MICA_SOURCE_REPO:-}" ]; then
        REPO_NAME="${MICA_SOURCE_REPO}"
    else
        local origin_url
        origin_url="$(git -C "${REGISTRY_REPO_ROOT}" remote get-url origin 2>/dev/null || true)"
        REPO_NAME="$(basename "${origin_url%/}" .git)"
        [ -n "${origin_url}" ] && [ -n "${REPO_NAME}" ] || {
            echo "error: ${REGISTRY_REPO_ROOT} has no 'origin' remote, so the repository name cannot be derived; set MICA_SOURCE_REPO=<name>" >&2
            return 1
        }
    fi
    [[ "${REPO_NAME}" =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ ]] || { echo "error: '${REPO_NAME}' is not a plain repository name" >&2; return 1; }
}

# Every package pin, validated, as TAB-separated rows on stdout:
#   package version arch sha256 source-repo source-commit
# one row per archive; with an architecture, only that arch and `all`.
#
# A pin is deps/packages/<package>.json:
#   { "name": "mica-podman", "repository": "mica", "commit": "<40 hex>",
#     "targets": { "amd64": { "version": "...", "architecture": "amd64",
#                             "sha256": "<64 hex>", "asset": "mica-podman_..._amd64.deb" },
#                  "arm64": { ... } } }
# The asset is the archive's name (a pin written against a GitHub Release
# carries it with `+` as `.`, as that API renamed it; both are accepted);
# the sha256 is the blob's digest in the repository's pool artifact.
lock_rows() {
    local want="${1:-}"
    [ -d "${LOCK_DIR}" ] || { echo "error: ${LOCK_DIR} does not exist; it holds the package pins and every reader needs it, even empty" >&2; return 1; }
    local file
    for file in "${LOCK_DIR}"/*.json; do
        [ -e "${file}" ] || continue
        jq -e --arg stem "$(basename "${file}" .json)" '
            type == "object" and (keys | sort) == ["commit", "name", "repository", "targets"]
            and .name == $stem and (.name | test("^[a-z0-9][a-z0-9+.-]+$"))
            and (.repository | test("^[A-Za-z0-9][A-Za-z0-9._-]*$")) and (.commit | test("^[0-9a-f]{40}$"))
            and (.targets | type == "object" and length > 0 and (keys - ["amd64", "arm64"] | length == 0))
            and ([.targets | to_entries[] | .key as $pool | .value
                  | (type == "object" and (keys | sort) == ["architecture", "asset", "sha256", "version"])
                    and (.architecture == $pool or .architecture == "all")
                    and (.version | test("^[0-9][A-Za-z0-9.~+-]*\\+git[0-9a-f]{12}-[1-9][0-9]*$"))
                    and (.sha256 | test("^[0-9a-f]{64}$"))
                    and ((.asset == ($stem + "_" + .version + "_" + .architecture + ".deb"))
                         or (.asset == (($stem + "_" + .version + "_" + .architecture + ".deb") | gsub("\\+"; "."))))]
                 | all)' "${file}" >/dev/null 2>&1 || {
            echo "error: ${file} is not a package pin: an object named after the file with repository, a 40-hex commit and targets keyed amd64/arm64, each with version (clean git stamp), architecture (the pool's or all), sha256 and the asset name (<package>_<version>_<arch>.deb with + as .)" >&2
            return 1
        }
        jq -r '.name as $n | .repository as $r | .commit as $c | .targets | to_entries[] | .value | [$n, .version, .architecture, .sha256, $r, $c] | @tsv' "${file}"
    done | LC_ALL=C sort -u | awk -F'\t' -v want="${want}" '
        {
            key = $1 "\t" $3
            if (key in seen) { printf "error: %s is pinned twice for %s with different targets\n", $1, $3 > "/dev/stderr"; bad = 1; exit 1 }
            seen[key] = 1
            if (want == "" || $3 == want || $3 == "all") print
        }
        END { if (bad) exit 1 }
    '
}

#!/usr/bin/env bash
# Turn the IMAGE_ keys of the pinned mica-build-env release's images.env into
# the --build-arg lines that carry a `FROM` into a Dockerfile, and refuse
# everything that must not reach one. This repository's own copy of the
# release's from.sh (RULES.md: each repository implements the rules in its own
# scripts), reading build-env/images.env, which scripts/build/build-env.sh
# fetched and verified at the pin in deps/build-env.json.
#
#   bash scripts/build/from.sh MICA_BUILD_BASE=IMAGE_MICA_BUILD_BASE
#       -> --build-arg MICA_BUILD_BASE=ghcr.io/ybolab/mica-build-env:base.inputs-...@sha256:...
#   bash scripts/build/from.sh --ref IMAGE_MICA_BUILD_RUST
#       -> ghcr.io/ybolab/mica-build-env:rust.inputs-...@sha256:...
#   bash scripts/build/from.sh --check
#       -> validate every IMAGE_ key, print nothing, exit 0 or 1
#
# Only IMAGE_ keys, each a digest pin of a multi-architecture index; LOCAL_
# keys are refused. It builds and pulls nothing; --arch= is accepted for the
# callers that name the architecture they stand on, and the index digest
# covers both.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${HERE}/../.." && pwd)"
IMAGES_ENV="${REPO_ROOT}/build-env/images.env"

for p in "${REPO_ROOT}/Makefile" "${IMAGES_ENV}"; do
    [ -e "${p}" ] || {
        echo "error: ${p} does not exist. scripts/build/from.sh derives REPO_ROOT as two levels above itself and reads the release images.env there; fetch it with: make deps" >&2
        exit 1
    }
done

STRIPPED="$(sed -e 's/[[:space:]]*#.*$//' -e '/^[[:space:]]*$/d' "${IMAGES_ENV}")"
[ -n "${STRIPPED}" ] || {
    echo "error: ${IMAGES_ENV} carries no assignments once comments are stripped, so every FROM below it would be handed an empty string" >&2
    exit 1
}
# shellcheck disable=SC1090
. <(printf '%s\n' "${STRIPPED}")

check_image_key() {
    local k="$1" v="$2"
    case "${v}" in
    *PENDING*)
        echo "error: ${k}=${v} in build-env/images.env is still PENDING, so there is no recorded base image to build on. A release never carries one; this images.env is not the verified release asset (make deps-check)" >&2
        return 1
        ;;
    esac
    if [ "${v#*@}" = "${v}" ]; then
        echo "error: ${k}=${v} in build-env/images.env names a TAG and not a digest. A tag is repointed by upstream whenever it rebuilds; pin it as name:tag@sha256:<64 hex>" >&2
        return 1
    fi
    if ! [[ "${v}" =~ ^[a-z0-9][a-z0-9._/-]*:[A-Za-z0-9._-]+@sha256:[0-9a-f]{64}$ ]]; then
        echo "error: ${k}=${v} in build-env/images.env is not a well-formed digest pin. Expected name:tag@sha256: followed by exactly 64 lowercase hex digits" >&2
        return 1
    fi
    return 0
}

# One key to its validated value; every output mode goes through here.
resolve_key() {
    local key="$1" val
    val="${!key-}"
    if [ -z "${val}" ]; then
        echo "error: build-env/images.env defines no ${key}. Every base image in this tree is a key in that file; if this is a new one, add it there rather than writing it into a FROM or a docker run" >&2
        return 1
    fi
    case "${key}" in
    IMAGE_*) check_image_key "${key}" "${val}" || return 1 ;;
    *)
        echo "error: ${key} is not an IMAGE_ key. This repository builds on the published images of its mica-build-env release, pinned by digest; a LOCAL_ key names an image mica-build-env builds on a host, which RULES.md keeps out of the release contract" >&2
        return 1
        ;;
    esac
    printf '%s\n' "${val}"
}

# --check: every IMAGE_ key, whether or not this run consumes it.
if [ "${1-}" = "--check" ]; then
    [ "$#" -eq 1 ] || { echo "error: --check takes no other arguments" >&2; exit 1; }
    bad=0
    seen=0
    while IFS= read -r k; do
        [ "${k#IMAGE_}" != "${k}" ] || continue
        seen=$((seen + 1))
        check_image_key "${k}" "${!k-}" || bad=1
    done < <(printf '%s\n' "${STRIPPED}" | sed -n 's/^\([A-Za-z_][A-Za-z0-9_]*\)=.*/\1/p')
    [ "${seen}" -gt 0 ] || {
        echo "error: build-env/images.env defines no IMAGE_ key at all, so this check passed by having nothing to check" >&2
        exit 1
    }
    exit "${bad}"
fi

if [ "${1-}" != "${1#--arch=}" ]; then
    case "${1#--arch=}" in
    amd64 | arm64) ;;
    *)
        echo "error: $1 names no architecture the published images carry; every IMAGE_MICA_BUILD_* index holds amd64 and arm64" >&2
        exit 1
        ;;
    esac
    shift
fi

# --ref: exactly one bare reference, for `docker run` call sites.
if [ "${1-}" = "--ref" ]; then
    shift
    [ "$#" -eq 1 ] || {
        echo "error: --ref takes exactly one build-env/images.env key. It prints one reference on stdout; a call with none would print an empty string that a caller would substitute into a docker command line as no image at all, and a call with several would have to be read back by position" >&2
        exit 1
    }
    resolve_key "$1"
    exit 0
fi

[ "$#" -gt 0 ] || {
    cat >&2 <<'USAGE'
usage: from.sh [--arch=<amd64|arm64>] <ARG_NAME>=<IMAGE_KEY> [...]
       from.sh [--arch=<amd64|arm64>] --ref <IMAGE_KEY>
       from.sh --check

A call with no pairs would print nothing and exit 0, and a caller that
substituted that into a docker command line would build with no --build-arg at
all -- which is precisely the unpinned build this file exists to prevent.
USAGE
    exit 1
}

bad=0
out=()
for pair in "$@"; do
    arg="${pair%%=*}"
    key="${pair#*=}"
    if [ "${arg}" = "${pair}" ] || [ -z "${arg}" ] || [ -z "${key}" ]; then
        echo "error: '${pair}' is not <ARG_NAME>=<IMAGES_ENV_KEY>" >&2
        bad=1
        continue
    fi
    # Keep going so every bad key is reported, not just the first.
    if ! val="$(resolve_key "${key}")"; then
        bad=1
        continue
    fi
    out+=(--build-arg "${arg}=${val}")
done
[ "${bad}" = 0 ] || exit 1

printf '%s\n' "${out[@]}"

#!/usr/bin/env bash
# The version release of this repository: a tag v<VERSION> on a commit of main.
#
#   bash scripts/build/release.sh check <tag>     the tag may be released from HEAD
#   bash scripts/build/release.sh publish <tag>   the GitHub release of that tag
#
# Releasing is manual: bump VERSION by a commit on main, then push the tag
# (`git tag v<VERSION> <commit> && git push origin v<VERSION>`) or dispatch the
# release workflow with that tag. Nothing is published by a branch push.
#
# check refuses a tag that is not v<X.Y.Z>, that does not equal v$(cat VERSION)
# at HEAD, that does not name HEAD, or whose commit is not on origin/main.
#
# publish runs after the pool of HEAD was published and read back
# (scripts/deb/publish.sh). It creates the GitHub release <tag> as a draft
# whose notes name the source commit, both pool artifacts by manifest digest
# and every archive by sha256, publishes it, and reads it back with no
# credential. An existing release is never changed: publish refuses one.
# GH_TOKEN is the workflow's token; nothing prints it.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${HERE}/../.." && pwd)"
die() { echo "release.sh: error: $*" >&2; exit 1; }
g() { git -C "${REPO_ROOT}" "$@"; }

[ "$#" -eq 2 ] || die "usage: bash scripts/build/release.sh check|publish <tag>"
MODE="$1"
TAG="$2"

[[ "${TAG}" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "'${TAG}' is not a release tag v<X.Y.Z>"
HEAD_COMMIT="$(g rev-parse --verify HEAD^{commit})"
VERSION="$(g show HEAD:VERSION | tr -d '[:space:]')"
[ "${TAG}" = "v${VERSION}" ] || die "the tag is ${TAG}, and VERSION at ${HEAD_COMMIT:0:12} says ${VERSION}; a release tag is v<VERSION>, bumped by a commit first"
TAG_COMMIT="$(g rev-parse --verify --quiet "refs/tags/${TAG}^{commit}")" || die "there is no tag ${TAG} in this checkout"
[ "${TAG_COMMIT}" = "${HEAD_COMMIT}" ] || die "${TAG} names ${TAG_COMMIT:0:12}, and the checkout is ${HEAD_COMMIT:0:12}"
case "${MODE}" in
check)
    g rev-parse --verify --quiet refs/remotes/origin/main >/dev/null || die "origin/main is not in this checkout, so ${TAG} cannot be shown to be on main"
    g merge-base --is-ancestor "${TAG_COMMIT}" refs/remotes/origin/main || die "${TAG} (${TAG_COMMIT:0:12}) is not a commit of origin/main"
    echo "release.sh: ${TAG} is VERSION ${VERSION} at ${TAG_COMMIT} on main"
    ;;
publish)
    for t in gh curl jq sha256sum; do command -v "${t}" >/dev/null 2>&1 || die "${t} is required and not on PATH"; done
    [ -n "${GH_TOKEN:-}" ] || die "GH_TOKEN is unset; creating a release needs the workflow token"
    REPO="${GITHUB_REPOSITORY:-ybolab/mica-core}"
    C12="${HEAD_COMMIT:0:12}"
    if gh release view "${TAG}" --repo "${REPO}" >/dev/null 2>&1; then
        die "the release ${TAG} already exists; a release is never changed -- bump VERSION and tag again"
    fi

    # The pool artifacts, read back anonymously: their digests go into the notes.
    token="$(curl -fsS "https://ghcr.io/token?service=ghcr.io&scope=repository:${REPO}:pull" | jq -r .token)"
    [ -n "${token}" ] && [ "${token}" != null ] || die "ghcr.io issued no anonymous pull token for ${REPO}"
    WORK="$(mktemp -d)"
    trap 'rm -rf "${WORK}"' EXIT
    {
        echo "Source: ${REPO}@${HEAD_COMMIT}"
        echo
        echo "Debian packages, version ${VERSION}+git${C12}-1, published as OCI pools:"
        echo
    } >"${WORK}/notes.md"
    for arch in amd64 arm64; do
        ref="pool.${arch}.build-${C12}"
        code="$(curl -sS -o "${WORK}/${arch}.json" -D "${WORK}/${arch}.hdr" -w '%{http_code}' \
            -H "Authorization: Bearer ${token}" -H 'Accept: application/vnd.oci.image.manifest.v1+json' \
            "https://ghcr.io/v2/${REPO}/manifests/${ref}")"
        [ "${code}" = 200 ] || die "ghcr.io/${REPO}:${ref} answered HTTP ${code} anonymously; the pool of ${C12} is not published"
        digest="sha256:$(sha256sum "${WORK}/${arch}.json" | cut -d' ' -f1)"
        jq -e --arg c "${HEAD_COMMIT}" --arg a "${arch}" \
            '.artifactType == "application/vnd.mica.pool" and .annotations["mica.source-commit"] == $c and .annotations["mica.arch"] == $a' \
            "${WORK}/${arch}.json" >/dev/null || die "ghcr.io/${REPO}:${ref} is not the ${arch} pool of ${HEAD_COMMIT}"
        {
            echo "- \`ghcr.io/${REPO}:${ref}@${digest}\`"
            jq -r '.layers[] | "  - `" + .annotations["org.opencontainers.image.title"] + "` " + .digest' "${WORK}/${arch}.json"
        } >>"${WORK}/notes.md"
    done

    gh release create "${TAG}" --repo "${REPO}" --verify-tag --title "${TAG}" --draft --notes-file "${WORK}/notes.md" >/dev/null
    gh release edit "${TAG}" --repo "${REPO}" --draft=false >/dev/null

    # Read back with no credential.
    code="$(curl -sS -o "${WORK}/release.json" -w '%{http_code}' "https://api.github.com/repos/${REPO}/releases/tags/${TAG}")"
    [ "${code}" = 200 ] || die "the release ${TAG} answered HTTP ${code} anonymously after publishing"
    jq -e --arg t "${TAG}" '.tag_name == $t and .draft == false' "${WORK}/release.json" >/dev/null ||
        die "the release ${TAG} read back anonymously is not the published ${TAG}"
    [ "$(jq -r .body "${WORK}/release.json" | tr -d '\r')" = "$(tr -d '\r' <"${WORK}/notes.md")" ] ||
        die "the release ${TAG} read back anonymously does not carry the notes that were written"
    echo "release.sh: ${TAG} published as $(jq -r .html_url "${WORK}/release.json")"
    ;;
*) die "usage: bash scripts/build/release.sh check|publish <tag>" ;;
esac

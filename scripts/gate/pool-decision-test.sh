#!/usr/bin/env bash
# scripts/build/pool-decision.sh against fixtures: a throwaway git history, a
# fixture registry directory and file:// releases. No network, no docker, no
# package build. The last case runs the decision over this repository's own
# history from the last published pool (74235c3) to d482bcc.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
DECISION="${REPO_ROOT}/scripts/build/pool-decision.sh"
for t in git jq curl sha256sum tar; do
    command -v "${t}" >/dev/null 2>&1 || { echo "error: ${t} is required and not on PATH" >&2; exit 1; }
done
mkdir -p "${REPO_ROOT}/tmp"
T="$(mktemp -d "${REPO_ROOT}/tmp/pool-decision-test.XXXXXX")"
trap 'rm -rf "${T}"' EXIT

PASS=0
FAIL=0
RUST_A="ghcr.io/ybolab/mica-build-env:rust.inputs-aaaaaaaaaaaaaaaa@sha256:$(printf 'a%.0s' {1..64})"
RUST_B="ghcr.io/ybolab/mica-build-env:rust.inputs-bbbbbbbbbbbbbbbb@sha256:$(printf 'b%.0s' {1..64})"
BASE_A="ghcr.io/ybolab/mica-build-env:base.inputs-cccccccccccccccc@sha256:$(printf 'c%.0s' {1..64})"
BASE_B="ghcr.io/ybolab/mica-build-env:base.inputs-dddddddddddddddd@sha256:$(printf 'd%.0s' {1..64})"
GO_A="ghcr.io/ybolab/mica-build-env:go.inputs-eeeeeeeeeeeeeeee@sha256:$(printf 'e%.0s' {1..64})"
GO_B="ghcr.io/ybolab/mica-build-env:go.inputs-ffffffffffffffff@sha256:$(printf 'f%.0s' {1..64})"

# ---- releases: <version> <rust> <base> <go>; prints the SHA256SUMS hash ----
REL="${T}/releases"
release() {
    local d="${REL}/$1"
    mkdir -p "${d}"
    printf 'fixture %s\n' "$1" | gzip -cn >"${d}/mica-build-env-$1.tar.gz"
    printf '# fixture\nIMAGE_MICA_BUILD_RUST=%s\nIMAGE_MICA_BUILD_BASE=%s\nIMAGE_MICA_BUILD_GO=%s\n' "$2" "$3" "$4" >"${d}/images.env"
    (cd "${d}" && sha256sum "mica-build-env-$1.tar.gz" images.env >SHA256SUMS)
    sha256sum "${d}/SHA256SUMS" | cut -d' ' -f1
}
S1="$(release v9.9.1 "${RUST_A}" "${BASE_A}" "${GO_A}")"
S2="$(release v9.9.2 "${RUST_A}" "${BASE_A}" "${GO_B}")" # same effective images, other version and GO
S3="$(release v9.9.3 "${RUST_B}" "${BASE_A}" "${GO_A}")" # Rust image changed
S4="$(release v9.9.4 "${RUST_A}" "${BASE_B}" "${GO_A}")" # base image changed
S5="$(release v9.9.5 "${RUST_A}" "${BASE_A}" "${GO_A}")"
S6="$(release v9.9.6 "${RUST_A/rust.inputs-aaaaaaaaaaaaaaaa/rust.build-0123456789ab}" "${BASE_A/base.inputs-cccccccccccccccc/base.inputs-1111111111111111}" "${GO_A}")" # other tags, same digests
S7="$(release v9.9.7 "${RUST_A%@*}@sha256:$(printf '7%.0s' {1..64})" "${BASE_A}" "${GO_A}")"                               # same Rust tag, other digest
S8="$(release v9.9.8 "ghcr.io/ybolab/other-env:rust.inputs-aaaaaaaaaaaaaaaa@${RUST_A#*@}" "${BASE_A}" "${GO_A}")"          # same Rust digest, other repository
S9="$(release v9.9.9 "${RUST_A%@*}" "${BASE_A}" "${GO_A}")"                                                               # verified, but the Rust ref is unpinned
S10="$(release v9.9.10 "${RUST_A}" "${BASE_A%@*}@sha256:CCCC" "${GO_A}")"                                                  # verified, but the base digest is malformed
echo tampered >>"${REL}/v9.9.5/images.env"                # SHA256SUMS no longer holds
pin() { printf '{\n  "version": "%s",\n  "sha256sums": "%s"\n}\n' "$1" "$2"; }

# ---- a git history ----
G="${T}/repo"
git init -q -b main "${G}"
gg() { git -C "${G}" -c user.name=fixture -c user.email=fixture@example.invalid -c commit.gpgsign=false "$@"; }
put() { mkdir -p "$(dirname "${G}/$1")"; printf '%s\n' "$2" >"${G}/$1"; }
commit() { gg add -A >/dev/null; gg commit -q --allow-empty -m "$1"; gg rev-parse HEAD; }
put crates/a/src/lib.rs 'pub fn a() {}'
put crates/a/README.md 'a'
put docs/plan/x.md 'x'
put README.md 'readme'
put .gitignore 'target/'
put .github/workflows/release.yml 'name: release'
put deps/build-env.json "$(pin v9.9.1 "${S1}")"
C0="$(commit base)"
branch() { gg checkout -q -B "t$RANDOM$RANDOM" "$1"; }

# ---- the registry fixture ----
# manifest <dir> <arch> <commit> [repo] [artifactType] [title-arch] [annotated-commit]
manifest() {
    local dir="$1" arch="$2" commit="$3" repo="${4:-mica-core}" type="${5:-application/vnd.mica.pool}" tarch="${6:-$2}" acommit="${7:-$3}"
    mkdir -p "${dir}/manifests"
    jq -n --arg type "${type}" --arg repo "${repo}" --arg c "${acommit}" --arg arch "${arch}" --arg title "micad_0.1.0+git${acommit:0:12}-1_${tarch}.deb" '
        {schemaVersion: 2, mediaType: "application/vnd.oci.image.manifest.v1+json", artifactType: $type,
         config: {mediaType: "application/vnd.oci.empty.v1+json", digest: "sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a", size: 2},
         layers: [{mediaType: "application/vnd.mica.deb", digest: ("sha256:" + ("0" * 64)), size: 1, annotations: {"org.opencontainers.image.title": $title}}],
         annotations: {"org.opencontainers.image.revision": $c, "org.opencontainers.image.created": "2026-09-13T00:00:00Z",
                       "org.opencontainers.image.source": ("https://github.com/ybolab/" + $repo), "mica.source-repo": $repo,
                       "mica.source-commit": $c, "mica.arch": $arch}}' >"${dir}/manifests/pool.${arch}.build-${commit:0:12}.json"
    echo "pool.${arch}.build-${commit:0:12}" >>"${dir}/tags"
}
pool() { manifest "$1" amd64 "$2"; manifest "$1" arm64 "$2"; } # <dir> <commit>
registry() { local d="${T}/reg-$1"; rm -rf "${d}"; mkdir -p "${d}/status"; : >"${d}/tags"; echo "${d}"; }

# expect <build|skip> <case> <reason substring> <registry dir> <head> [git dir]
expect() {
    local want="$1" name="$2" needle="$3" reg="$4" head="$5" git="${6:-${G}}" out line got
    out="${T}/out"
    : >"${T}/gh-output"
    GITHUB_OUTPUT="${T}/gh-output" MICA_POOL_DECISION_REGISTRY_DIR="${reg}" MICA_BUILD_ENV_RELEASES="file://${REL}" \
        bash "${DECISION}" --git "${git}" --head "${head}" >"${out}" 2>&1 || { FAIL=$((FAIL + 1)); echo "FAIL: ${name}: the decision exited non-zero"; sed 's/^/    /' "${out}"; return; }
    line="$(grep '^pool-decision: ' "${out}" | tail -n1)"
    got="$(cat "${T}/gh-output")"
    if [ "${got}" = "pool=${want}" ] && [ "${line#pool-decision: "${want}": }" != "${line}" ] && [ "${line#*"${needle}"}" != "${line}" ]; then
        PASS=$((PASS + 1))
        echo "PASS: ${name} -> ${line#pool-decision: }"
    else
        FAIL=$((FAIL + 1))
        echo "FAIL: ${name}: wanted pool=${want} with '${needle}', got [${got}]"
        sed 's/^/    /' "${out}"
    fi
}

# docs, markdown and .gitignore only
R="$(registry valid)"; pool "${R}" "${C0}"
branch "${C0}"; put docs/plan/x.md 'x2'; put README.md 'readme2'; put crates/a/README.md 'a2'; put .gitignore 'target/
_out/'; H="$(commit docs)"
expect skip "docs, markdown and .gitignore only" "only docs/markdown/.gitignore changed since ${C0:0:12}" "${R}" "${H}"
expect skip "HEAD is the published baseline" "nothing changed since ${C0:0:12}" "${R}" "${C0}"

# the release pin
branch "${C0}"; put deps/build-env.json "$(pin v9.9.2 "${S2}")"; put docs/plan/x.md 'pin note'; H="$(commit pin-same)"
expect skip "pin version bump with the same Rust and base digests (GO differs, unused)" "pin IMAGE_MICA_BUILD_RUST and IMAGE_MICA_BUILD_BASE to the same repository and digest" "${R}" "${H}"
branch "${C0}"; put deps/build-env.json "$(pin v9.9.6 "${S6}")"; H="$(commit pin-retagged)"
expect skip "pin whose Rust and base refs change tag but keep their digests" "pin IMAGE_MICA_BUILD_RUST and IMAGE_MICA_BUILD_BASE to the same repository and digest" "${R}" "${H}"
branch "${C0}"; put deps/build-env.json "$(pin v9.9.7 "${S7}")"; H="$(commit pin-same-tag-new-digest)"
expect build "pin whose Rust ref keeps its tag and changes digest" "IMAGE_MICA_BUILD_RUST changed" "${R}" "${H}"
branch "${C0}"; put deps/build-env.json "$(pin v9.9.8 "${S8}")"; H="$(commit pin-other-repository)"
expect build "pin whose Rust digest moved to another repository" "IMAGE_MICA_BUILD_RUST changed" "${R}" "${H}"
branch "${C0}"; put deps/build-env.json "$(pin v9.9.9 "${S9}")"; H="$(commit pin-unpinned)"
expect build "verified release with an unpinned Rust ref" "IMAGE_MICA_BUILD_RUST is missing or not a digest pin" "${R}" "${H}"
branch "${C0}"; put deps/build-env.json "$(pin v9.9.10 "${S10}")"; H="$(commit pin-malformed)"
expect build "verified release with a malformed base digest" "IMAGE_MICA_BUILD_BASE is missing or not a digest pin" "${R}" "${H}"
branch "${C0}"; put deps/build-env.json "$(pin v9.9.3 "${S3}")"; H="$(commit pin-rust)"
expect build "pin whose Rust image changed" "IMAGE_MICA_BUILD_RUST changed" "${R}" "${H}"
branch "${C0}"; put deps/build-env.json "$(pin v9.9.4 "${S4}")"; H="$(commit pin-base)"
expect build "pin whose base image changed" "IMAGE_MICA_BUILD_BASE changed" "${R}" "${H}"
branch "${C0}"; put deps/build-env.json "$(pin v9.9.5 "${S5}")"; H="$(commit pin-tampered)"
expect build "pin whose release assets do not verify" "did not verify" "${R}" "${H}"
branch "${C0}"; put deps/build-env.json "$(pin v9.9.2 "$(printf '0%.0s' {1..64})")"; H="$(commit pin-wrong-sha)"
expect build "pin recording the wrong SHA256SUMS hash" "did not verify" "${R}" "${H}"
branch "${C0}"; put deps/build-env.json "$(pin v9.9.99 "${S1}")"; H="$(commit pin-no-release)"
expect build "pin naming a release that cannot be fetched" "did not verify" "${R}" "${H}"

# real inputs
branch "${C0}"; put crates/a/src/lib.rs 'pub fn a() -> u8 { 1 }'; put docs/plan/x.md 'y'; H="$(commit input)"
expect build "a crate source change beside docs" "crates/a/src/lib.rs changed" "${R}" "${H}"
branch "${C0}"; put .github/workflows/release.yml 'name: release2'; H="$(commit workflow)"
expect build "a workflow change" ".github/workflows/release.yml changed" "${R}" "${H}"
branch "${C0}"; put Cargo.lock 'lock'; H="$(commit lock)"
expect build "a new Cargo.lock" "Cargo.lock changed" "${R}" "${H}"
branch "${C0}"; gg mv docs/plan/x.md crates/a/x.rs; H="$(commit rename-into-crate)"
expect build "a docs file renamed into a crate" "crates/a/x.rs changed" "${R}" "${H}"

# baselines
branch "${C0}"; put docs/plan/x.md 'docs'; HD="$(commit docs-for-baselines)"
expect build "first publication: no pool at all" "first publication" "$(registry empty)" "${HD}"
R2="$(registry amd64-only)"; manifest "${R2}" amd64 "${C0}"
expect build "only the amd64 pool was published" "first publication" "${R2}" "${HD}"
R2="$(registry arm64-404)"; pool "${R2}" "${C0}"; rm "${R2}/manifests/pool.arm64.build-${C0:0:12}.json"
expect build "arm64 tag listed, manifest 404" "no published ancestor" "${R2}" "${HD}"
R2="$(registry wrong-repo)"; manifest "${R2}" amd64 "${C0}"; manifest "${R2}" arm64 "${C0}" mica-system
expect build "arm64 pool of another repository" "no published ancestor" "${R2}" "${HD}"
R2="$(registry wrong-type)"; manifest "${R2}" amd64 "${C0}" mica-core application/vnd.mica.source; manifest "${R2}" arm64 "${C0}"
expect build "amd64 artifact that is not a pool" "no published ancestor" "${R2}" "${HD}"
R2="$(registry wrong-title)"; manifest "${R2}" amd64 "${C0}"; manifest "${R2}" arm64 "${C0}" mica-core application/vnd.mica.pool amd64
expect build "arm64 pool holding an amd64 archive" "no published ancestor" "${R2}" "${HD}"
R2="$(registry wrong-commit)"; manifest "${R2}" amd64 "${C0}"; manifest "${R2}" arm64 "${C0}" mica-core application/vnd.mica.pool arm64 "${C0:0:12}$(printf '9%.0s' {1..28})"
expect build "arm64 manifest naming another full commit" "no published ancestor" "${R2}" "${HD}"
R2="$(registry manifest-500)"; pool "${R2}" "${C0}"; echo 500 >"${R2}/status/pool.arm64.build-${C0:0:12}"
expect build "manifest read failure" "answered HTTP 500" "${R2}" "${HD}"
R2="$(registry tags-403)"; pool "${R2}" "${C0}"; echo 403 >"${R2}/status/tags"
expect build "tag listing failure" "could not be listed" "${R2}" "${HD}"
branch "${C0}"; put crates/a/src/lib.rs 'side'; S="$(commit side)"
R2="$(registry non-ancestor)"; pool "${R2}" "${S}"
expect build "a complete pool of a commit that is not an ancestor" "first publication" "${R2}" "${HD}"

# the nearest valid ancestor, not the oldest and not an invalid newer one
branch "${C0}"; put crates/a/src/lib.rs 'pub fn a() -> u8 { 2 }'; C1="$(commit input-published)"
put docs/plan/x.md 'after C1'; H="$(commit docs-after-c1)"
R2="$(registry nearest)"; pool "${R2}" "${C0}"; pool "${R2}" "${C1}"
expect skip "the nearest complete pool is the baseline" "since ${C1:0:12}" "${R2}" "${H}"
R2="$(registry newer-invalid)"; pool "${R2}" "${C0}"; manifest "${R2}" amd64 "${C1}"; manifest "${R2}" arm64 "${C1}" mica-system
expect build "a newer invalid pool falls back to an older baseline, whose diff holds the crate" "crates/a/src/lib.rs changed since ${C0:0:12}" "${R2}" "${H}"

# a baseline whose own release pin no longer verifies
branch "${C0}"; put deps/build-env.json "$(pin v9.9.5 "${S5}")"; CB="$(commit baseline-tampered-pin)"
put deps/build-env.json "$(pin v9.9.1 "${S1}")"; H="$(commit head-good-pin)"
R2="$(registry baseline-pin)"; pool "${R2}" "${CB}"
expect build "the baseline's release pin does not verify" "the release pin at ${CB:0:12} did not verify" "${R2}" "${H}"

# a baseline before the release pin existed
G0="${G}"
G="${T}/repo-nopin"
git init -q -b main "${G}"
put crates/a/src/lib.rs 'pub fn a() {}'
B0="$(commit no-pin)"
put deps/build-env.json "$(pin v9.9.1 "${S1}")"; H="$(commit add-pin)"
R2="$(registry nopin)"; pool "${R2}" "${B0}"
expect build "the baseline has no release pin" "does not exist at ${B0:0:12}" "${R2}" "${H}"
G="${G0}"

# the decision itself failing
expect build "an unknown head revision" "is not a commit" "${R}" "no-such-revision"

# this repository: the last published pool 74235c3 to the migration candidate d482bcc
if git -C "${REPO_ROOT}" rev-parse --verify --quiet 74235c35ddf58297315fe7973b6b7c46303acfc5^{commit} >/dev/null &&
    git -C "${REPO_ROOT}" rev-parse --verify --quiet d482bcc27afc93ea5a2cdb5f6e840ca0bef8f28b^{commit} >/dev/null; then
    R2="$(registry mica-core-74235c3)"; pool "${R2}" 74235c35ddf58297315fe7973b6b7c46303acfc5
    expect build "mica-core d482bcc against its last published pool 74235c3" "changed since 74235c35ddf5 and is a package, build or workflow input" "${R2}" d482bcc27afc93ea5a2cdb5f6e840ca0bef8f28b "${REPO_ROOT}"
else
    FAIL=$((FAIL + 1))
    echo "FAIL: this checkout lacks 74235c3 or d482bcc, so the convergence case was not run"
fi

echo "RESULT: ${PASS} passed, ${FAIL} failed"
[ "${FAIL}" = 0 ]

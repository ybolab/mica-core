#!/usr/bin/env bash
# Compile one producer's crates out of the micad workspace, prove the producer
# boundary held, and stage the binaries for packaging.
#
#   bash hack/build-deb.sh --producer micad --crates micad \
#        --arch amd64 --stage <dir>
#
# WHY THIS FILE STILL EXISTS. Every producer in this repository is built by
# build-env/deb/build.sh from its producer.env, and this is not a second
# driver: it is the micad workspace's PREPARE hook implementation, reached
# through deb/<producer>/prepare.sh. What it does -- cross-compile
# a named set of crates and then assert that NOTHING ELSE was compiled with them
# -- is a claim about a cargo build, and no key in producer.env describes a
# cargo build. The generic driver packs what it is handed; this decides what it
# is handed and what may not be in it.
#
# THE COMPILE RUNS AT amd64 FOR BOTH TARGETS. cargo cross-compiles, so this is a
# plain `docker run` against localhost/mica-build-rust:amd64 and needs no buildx
# and no emulation. The PACKAGING runs at the target architecture, and that is
# build-env/deb/build.sh's half.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE="$(cd "${HERE}/.." && pwd)"
REPO_ROOT="${WORKSPACE}"
FROM_SH="${REPO_ROOT}/build-env/from.sh"
for p in "${WORKSPACE}/Cargo.toml" "${FROM_SH}"; do
    [ -e "${p}" ] || {
        echo "error: ${p} does not exist. hack/build-deb.sh derives the workspace as its own directory's parent, which is the repository; build-env/ is the mica-build-env source pin (make deps), and a missing one is refused here rather that; if this file moved, that arithmetic moved with it" >&2
        exit 1
    }
done

command -v docker >/dev/null 2>&1 || {
    echo "error: docker is required and not on PATH. The compile runs in a container -- the host carries no cargo -- which is what makes the compiler a value build-env/images.env records" >&2
    exit 1
}

PRODUCER=""
CRATES=""
ARCH=""
STAGE=""
while [ "$#" -gt 0 ]; do
    case "$1" in
    --producer)
        PRODUCER="${2-}"
        [ -n "${PRODUCER}" ] || { echo "error: --producer takes a producer name" >&2; exit 1; }
        shift 2
        ;;
    --crates)
        CRATES="${2-}"
        [ -n "${CRATES}" ] || { echo "error: --crates takes a space-separated crate list" >&2; exit 1; }
        shift 2
        ;;
    --arch)
        ARCH="${2-}"
        [ -n "${ARCH}" ] || { echo "error: --arch takes amd64 or arm64" >&2; exit 1; }
        shift 2
        ;;
    --stage)
        STAGE="${2-}"
        [ -n "${STAGE}" ] || { echo "error: --stage takes a directory" >&2; exit 1; }
        shift 2
        ;;
    *)
        echo "usage: bash hack/build-deb.sh --producer <name> --crates \"<crate> ...\" --arch <amd64|arm64> --stage <dir>" >&2
        exit 1
        ;;
    esac
done
[ -n "${PRODUCER}" ] || { echo "error: --producer is required; it names the producer-private target directory, and two producers sharing one would let a build satisfy the independence assertion below with binaries nobody asked for" >&2; exit 1; }
[ -n "${CRATES}" ] || { echo "error: --crates is required; there is no default crate list, because a build that picked one would compile a subset nobody asked for" >&2; exit 1; }
[ -n "${ARCH}" ] || { echo "error: --arch is required; guessing the host's would silently produce amd64 binaries for a cx3576 image" >&2; exit 1; }
[ -n "${STAGE}" ] || { echo "error: --stage is required; it is where build-env/deb/build.sh looks for what this hook produced" >&2; exit 1; }
[ -d "${STAGE}" ] || { echo "error: --stage ${STAGE} is not a directory. build-env/deb/build.sh creates it before running this hook" >&2; exit 1; }

BINARIES=()
for c in ${CRATES}; do BINARIES+=("${c}"); done

# Every binary this workspace can build. A producer names the ones it OWNS in
# its prepare.sh, and the complement is what the independence assertion looks
# for -- so a fifth binary added here is checked without any producer being
# edited.
ALL_BINARIES=(micad apid mica-mqttd mica-mqtt-broker mica-mqtt-reference)

# A crate this workspace does not build would make the complement below wrong in
# the direction that matters: it would be treated as owned, and therefore never
# looked for.
for name in "${BINARIES[@]}"; do
    known=0
    for a in "${ALL_BINARIES[@]}"; do
        [ "${name}" != "${a}" ] || known=1
    done
    [ "${known}" = 1 ] || {
        echo "error: '${name}' is not one of this workspace's binaries (${ALL_BINARIES[*]}). The independence assertion below checks the COMPLEMENT of what a producer owns, so an unknown name here would silently shrink what is checked" >&2
        exit 1
    }
done

# The complement of BINARIES: what this producer's target directory must not
# hold. Computed rather than listed, so it cannot fall behind ALL_BINARIES.
EXCLUDED=()
for name in "${ALL_BINARIES[@]}"; do
    owned=0
    for own in "${BINARIES[@]}"; do
        [ "${name}" != "${own}" ] || owned=1
    done
    [ "${owned}" = 1 ] || EXCLUDED+=("${name}")
done

case "${ARCH}" in
amd64)
    TRIPLE=x86_64-unknown-linux-gnu
    ELF_ARCH=x86-64
    ;;
arm64)
    TRIPLE=aarch64-unknown-linux-gnu
    ELF_ARCH=aarch64
    ;;
*)
    echo "error: --arch ${ARCH} is not amd64 or arm64. Those are the two architectures build-env/images.env pins a Rust std for; adding a third is an images.env edit and not an argument to this script" >&2
    exit 1
    ;;
esac

git -C "${REPO_ROOT}" rev-parse --git-dir >/dev/null 2>&1 || {
    echo "error: ${REPO_ROOT} is not a git checkout, so the commit these binaries report cannot be resolved" >&2
    exit 1
}
COMMIT="$(git -C "${REPO_ROOT}" rev-parse --short=12 HEAD)"
DIRTY=""
[ -z "$(git -C "${REPO_ROOT}" status --porcelain)" ] || DIRTY="-dirty"
# The commit the binaries themselves report, the same value and the same
# resolution hack/build-target.sh uses -- micad and apid answer
# --version with `<name> <crate version> (<commit>)`, and verify's smoke
# runner checks that against what the build embedded. This is NOT the package
# version: that is build-env/deb/version.sh's, and it is one rule for the
# whole pool.
MICA_BUILD_COMMIT="${MICA_BUILD_COMMIT:-${COMMIT}${DIRTY}}"

# Producer-private, so this producer has its own cache key and its own output
# directory: sharing target/ with build-target.sh would let a
# four-binary build satisfy the independence assertion below with binaries this
# producer never asked for. Gitignored through .gitignore.
TARGET_DIR="${WORKSPACE}/target-deb/${PRODUCER}"
RELEASE_DIR="${TARGET_DIR}/${TRIPLE}/release"

CARGO_CACHE="${REPO_ROOT}/_out/cargo"
mkdir -p "${CARGO_CACHE}/registry" "${CARGO_CACHE}/git" "${TARGET_DIR}"

mapfile -t RUST_FROM < <(bash "${FROM_SH}" --arch=amd64 MICA_BUILD_RUST=LOCAL_MICA_BUILD_RUST)
[ "${#RUST_FROM[@]}" -eq 2 ] || {
    echo "error: build-env/from.sh did not yield localhost/mica-build-rust:amd64 (see its message above); it is built by \`make build-env\`, which must run before any component build that stands on it" >&2
    exit 1
}
RUST_IMAGE="${RUST_FROM[1]#MICA_BUILD_RUST=}"

# Only the producer that owns micad -- whose binary links apid -- needs the frontend. Finish that separate
# producer before entering the Rust-only cross-build image.
APID_UI_ARGS=()
for name in "${BINARIES[@]}"; do
    if [ "${name}" = micad ]; then
        APID_UI_DIST="${REPO_ROOT}/_out/apid-ui/dist"
        bash "${WORKSPACE}/apid/ui/build.sh"
        APID_UI_ARGS=(
            -v "${APID_UI_DIST}:/build/apid-ui:ro"
            -e "MICA_APID_UI_DIST_DIR=/build/apid-ui"
        )
        break
    fi
done

# The repository at the fixed path /src, not where it happens to live, for the
# reason build-target.sh gives: rustc records the paths it is given, so mounting
# the checkout at its own path would make the binaries depend on the directory
# the repository was cloned into.
# mica-build-side: container-block -- the compiler is localhost/mica-build-rust's,
# recorded in that image's /etc/mica-build/rust.env, and this block refuses an image that
# carries no such record
docker run --rm \
    --label ai-agent=true \
    --platform linux/amd64 \
    -v "${REPO_ROOT}:/src:ro" \
    -v "${TARGET_DIR}:/target" \
    -v "${CARGO_CACHE}/registry:/usr/local/cargo/registry" \
    -v "${CARGO_CACHE}/git:/usr/local/cargo/git" \
    -w /src \
    -e "TARGET=${TRIPLE}" \
    -e "ELF_ARCH=${ELF_ARCH}" \
    -e "CRATES=${BINARIES[*]}" \
    -e "CARGO_TARGET_DIR=/target" \
    -e "MICA_BUILD_COMMIT=${MICA_BUILD_COMMIT}" \
    "${APID_UI_ARGS[@]}" \
    --entrypoint /bin/bash \
    "${RUST_IMAGE}" -c '
        set -euo pipefail
        [ -f /etc/mica-build/rust.env ] || {
            echo "error: this image carries no /etc/mica-build/rust.env, so what compiled these binaries cannot be read back out of it" >&2
            exit 1
        }
        . /etc/mica-build/rust.env
        echo "build-deb: compiling ${CRATES} for ${TARGET} with rustc ${MICA_BUILD_RUSTC} from ${MICA_BUILD_IMAGE}"
        # -p per crate and nothing else: the whole point of a producer is that
        # it cannot emit a binary it does not own. --locked makes Cargo.lock
        # the decision and refuses a build that would quietly update it.
        pkgs=""
        for c in ${CRATES}; do pkgs="${pkgs} -p ${c}"; done
        # shellcheck disable=SC2086
        cargo build --release --locked --target "${TARGET}" ${pkgs}
        for name in ${CRATES}; do
            bin="${CARGO_TARGET_DIR}/${TARGET}/release/${name}"
            [ -f "${bin}" ] || { echo "error: ${name} was not produced by the build" >&2; exit 1; }
            got="$(file -b "${bin}")"
            case "${got}" in
            *"ELF 64-bit"*"${ELF_ARCH}"*) ;;
            *) echo "error: ${name} is not an ${ELF_ARCH} ELF: ${got}" >&2; exit 1 ;;
            esac
        done
    '
# mica-build-side: host

# What this producer OWNS, checked before what it must not hold. Without this the
# scan below would pass over a directory the build never wrote -- an "is absent"
# assertion over an empty tree reports green forever.
for name in "${BINARIES[@]}"; do
    [ -f "${RELEASE_DIR}/${name}" ] || {
        echo "error: ${RELEASE_DIR}/${name} does not exist after the build. The compile reported success, so this is the export or the target directory and not the compiler" >&2
        exit 1
    }
done

# INDEPENDENCE, asserted rather than described. `cargo build -p micad` is
# the intent; this is the evidence, and it is what a later gate can point at. A
# binary here means the producer boundary leaked -- a stale target directory
# reused, or a -p list that grew.
stray=""
for name in "${EXCLUDED[@]}"; do
    while IFS= read -r f; do
        [ -z "${f}" ] || stray="${stray} ${f}"
    done < <(find "${TARGET_DIR}" -type f -name "${name}")
done
if [ -n "${stray}" ]; then
    echo "error: the ${PRODUCER} producer's target directory holds binaries it does not own:${stray}. Each producer compiles only its own crates into its own CARGO_TARGET_DIR; see deb/README.md" >&2
    exit 1
fi
echo "build-deb: ${TARGET_DIR} holds ${BINARIES[*]} and none of ${EXCLUDED[*]}"

# The staging directory rather than the release directory itself: a cargo target
# directory is gigabytes of intermediates, and buildx would walk all of it to
# find two files.
for name in "${BINARIES[@]}"; do
    cp "${RELEASE_DIR}/${name}" "${STAGE}/${name}"
done
echo "build-deb: staged ${BINARIES[*]} into ${STAGE}"

# NO BUILD RECORD IS WRITTEN HERE ANY MORE. The commit these binaries report is
# the commit pack.sh writes into the archive's Mica-Source-Commit control field,
# and rootfs/build.sh reads it out of the micad archive it installs to write
# _out/<board>/micad-build.txt for verify/src/smoke.ts. One source of the fact,
# carried inside the archive, so a package fetched from the registry has it
# exactly as a package built here does.

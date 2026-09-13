#!/usr/bin/env bash
# Cross-build micad, mica-apid, mica-mqttd and mica-mqtt-broker for one Rust target
# and verify the ELF.
#
#   bash scripts/build/build-target.sh <rust-target> <elf-arch-substring>
#   bash scripts/build/build-target.sh aarch64-unknown-linux-gnu aarch64
#   bash scripts/build/build-target.sh x86_64-unknown-linux-gnu  x86-64
#
# The compiler is localhost/mica-build-rust's, not the host's, so docker is the
# one thing the host needs: no rustup, no cross linker, no target std. What
# compiled these binaries is a value build-env/images.env records.
set -euo pipefail
cd "$(dirname "$0")/../.."
WORKSPACE="$(pwd)"
REPO_ROOT="${WORKSPACE}"
FROM_SH="${REPO_ROOT}/build-env/from.sh"

TARGET="${1:?usage: build-target.sh <rust-target> <elf-arch>}"
ELF_ARCH="${2:?usage: build-target.sh <rust-target> <elf-arch>}"

# rootfs/build.sh, hack/build-aarch64.sh and a hand invocation all
# reach this file, and a relative path resolves against whichever is the caller,
# so both roots are derived from $0.
for p in "${WORKSPACE}/Cargo.toml" "${FROM_SH}"; do
    [ -e "${p}" ] || {
        echo "error: ${p} does not exist. scripts/build/build-target.sh derives the workspace and the repository as two levels above itself; if this file moved, that arithmetic moved with it" >&2
        exit 1
    }
done

command -v docker >/dev/null 2>&1 || {
    echo "error: docker is required and not on PATH. This build runs inside localhost/mica-build-rust rather than on the host's cargo, which is what makes the compiler a value recorded in build-env/images.env instead of whatever the machine happens to have" >&2
    exit 1
}

# The image architecture is derived from the Rust target rather than taken as a
# third argument: a second way to say the same thing can disagree with itself,
# and the disagreement would look like a wrong-arch binary two builds later.
case "${TARGET}" in
aarch64-*) IMAGE_ARCH=amd64 ;; # cross-built FROM an amd64 builder
x86_64-*) IMAGE_ARCH=amd64 ;;
*)
    echo "error: '${TARGET}' is not a target build-env/images.env pins a Rust std for. mica-build-rust ships std for exactly two triples -- RUST_TRIPLE_AMD64 and RUST_TRIPLE_ARM64 -- and adding a third is an images.env edit (RUST_STD_SHA256_<arch>) and not an argument to this script" >&2
    exit 1
    ;;
esac

# The image, out of images.env, and refused by name if it is missing or is the
# wrong architecture -- docker reports the first as a failed pull from a
# registry called `localhost` and the second as a manifest error, neither of
# which names `make build-env`.
mapfile -t FROM_ARGS < <("${FROM_SH}" --arch="${IMAGE_ARCH}" MICA_BUILD_RUST=LOCAL_MICA_BUILD_RUST)
[ "${#FROM_ARGS[@]}" -eq 2 ] || {
    echo "error: build-env/from.sh did not yield localhost/mica-build-rust (see its message above)" >&2
    exit 1
}
IMAGE="${FROM_ARGS[1]#MICA_BUILD_RUST=}"

# The frontend is a separate, container-only producer. Its source is mounted
# read-only and its generated tree stays outside the source checkout.
APID_UI_DIST="${REPO_ROOT}/_out/apid-ui/dist"
bash "${WORKSPACE}/crates/mica-apid/ui/build.sh"

# The cargo caches are repo-local bind mounts, not docker volumes and not
# $HOME/.cargo: `make clean` and `rm -rf _out` then mean what they say, and the
# host carries no Rust state at all. CARGO_HOME=/usr/local/cargo is the image's
# own setting, the same path mica-podman:Dockerfile's cargo cache mounts use, so
# the two Rust builds here warm the same directory layout without sharing the
# cache itself.
CARGO_CACHE="${REPO_ROOT}/_out/cargo"
mkdir -p "${CARGO_CACHE}/registry" "${CARGO_CACHE}/git" "${WORKSPACE}/target"

# The commit the binaries report is resolved on the host and handed in as an
# environment variable. micad and apid answer `--version` with `<name> <crate
# version> (<commit>)`, and the commit half cannot be discovered from inside the
# container: this checkout is a git worktree whose `.git` is a file naming a
# gitdir outside the mount, so git in the container reports `not a git
# repository` even though git is installed there. An already-resolved
# MICA_BUILD_COMMIT in the environment wins, so a release pipeline handed a commit
# or a rebuild of an exported tarball with no .git says so rather than being told
# it is `unknown`. An empty value is not an error: `-e MICA_BUILD_COMMIT=` sets
# the empty string, `option_env!` yields `Some("")`, and both crates report
# `unknown` and still exit 0. Dirty is marked, never passed off as the clean SHA,
# and the test is `git status --porcelain` rather than `git diff`: an
# untracked-but-not-ignored `.rs` file is compiled in exactly like a modified
# one.
if [ -z "${MICA_BUILD_COMMIT:-}" ]; then
    MICA_BUILD_COMMIT=""
    if command -v git >/dev/null 2>&1 &&
        git -C "${REPO_ROOT}" rev-parse --git-dir >/dev/null 2>&1; then
        MICA_BUILD_COMMIT="$(git -C "${REPO_ROOT}" rev-parse --short=12 HEAD 2>/dev/null || true)"
        if [ -n "${MICA_BUILD_COMMIT}" ] &&
            [ -n "$(git -C "${REPO_ROOT}" status --porcelain 2>/dev/null)" ]; then
            MICA_BUILD_COMMIT="${MICA_BUILD_COMMIT}-dirty"
        fi
    fi
fi
if [ -n "${MICA_BUILD_COMMIT}" ]; then
    echo "micad: embedding build commit ${MICA_BUILD_COMMIT}"
else
    echo "micad: no build commit could be resolved; micad and apid will report unknown" >&2
fi

# NO BUILD RECORD IS WRITTEN HERE, and that is a decision rather than an
# omission. This script used to write `_out/micad-build.txt` for the smoke runner
# to assert against; a composed root has never carried the binaries it produces.
# They come out of the micad and mica-apid packages, whose archives carry the
# commit they were built from in their Mica-Source-Commit control field
# (build-env/deb/pack.sh), and rootfs/build.sh reads that field into
# `_out/<board>/micad-build.txt`. A record from here would name the commit of a
# build whose output nothing installs, and would be indistinguishable from one
# that named the build that did. See RFCT-356.

# The repository is mounted, not . That used to be forced: Cargo.toml
# listed one workspace member outside this directory, and mounting  alone
# made cargo fail on a manifest it could not see. The split extracted that
# crate into its own workspace under pkgs/, so no member reaches outside
#  any more and that reason is gone.
#
# The decision is unchanged, because the reason below always carried it on its
# own and now carries it alone: at the fixed path /src, because rustc records
# the paths it is given:
# mounting the checkout where it happens to live would make the output depend on
# the directory the repository was cloned into -- two machines, same commit,
# different binaries, for a reason that is not about the source.
#
# The loop below is DORMANT, not dead: the members list has no `../` entry today,
# so it iterates zero times. It is kept because it is the general form -- the
# next member added outside  fails with a sentence rather than with a
# missing-file error that names the file and not the cause.
while IFS= read -r m; do
    [ -n "${m}" ] || continue
    case "${m}" in
    ../*) ;;
    *) continue ;;
    esac
    abs="$(cd "${WORKSPACE}" && cd "$(dirname "${m}")" 2>/dev/null && pwd)/$(basename "${m}")" || abs=""
    case "${abs}" in
    "${REPO_ROOT}"/*) ;;
    *)
        echo "error: Cargo.toml lists the workspace member '${m}', which resolves outside ${REPO_ROOT}. This build mounts the repository into the container and nothing above it, so cargo would report that member as a missing Cargo.toml rather than as a member the container cannot see" >&2
        exit 1
        ;;
    esac
done < <(sed -n 's/^members = \[\(.*\)\]/\1/p' "${WORKSPACE}/Cargo.toml" | tr ',' '\n' | tr -d ' "')

# `--network host` is not used and is not needed: cargo fetches through the
# container's default network, and the only thing bound in is this repository.
# mica-build-side: container-block -- the compiler is localhost/mica-build-rust's,
# recorded in that image's /etc/mica-build/rust.env, and this block refuses an image that
# carries no such record
docker run --rm \
    --platform "linux/${IMAGE_ARCH}" \
    -v "${REPO_ROOT}:/src:ro" \
    -v "${WORKSPACE}/target:/target" \
    -v "${APID_UI_DIST}:/build/apid-ui:ro" \
    -v "${CARGO_CACHE}/registry:/usr/local/cargo/registry" \
    -v "${CARGO_CACHE}/git:/usr/local/cargo/git" \
    -w /src \
    -e "TARGET=${TARGET}" \
    -e "CARGO_TARGET_DIR=/target" \
    -e "MICA_BUILD_COMMIT=${MICA_BUILD_COMMIT}" \
    -e "MICA_APID_UI_DIST_DIR=/build/apid-ui" \
    --entrypoint /bin/bash \
    "${IMAGE}" -c '
        set -euo pipefail
        # The image records what it is; this reads it back and prints it, so the
        # build log answers "which rustc compiled this" without anyone having to
        # know which image was current. An image with no record is one that
        # cannot answer that, and build-env/build.sh refuses to tag one.
        [ -f /etc/mica-build/rust.env ] || {
            echo "error: this image carries no /etc/mica-build/rust.env, so what compiled these binaries cannot be read back out of it" >&2
            exit 1
        }
        . /etc/mica-build/rust.env
        echo "micad: building ${TARGET} with rustc ${MICA_BUILD_RUSTC} from ${MICA_BUILD_IMAGE} (RUST_SHA256=${MICA_BUILD_RUST_SHA256})"
        # --locked makes Cargo.lock the decision and refuses a build that
        # would quietly update it.
        cargo build --release --locked --target "${TARGET}" \
            -p mica-core -p mica-apid -p mica-mqttd -p mica-mqtt-broker
    '
# mica-build-side: host

# The ELF check is per binary, not just the first: a target that silently
# produced a host-arch artifact for one crate would otherwise ship and fail at
# exec time on the device, one binary later than a wholly wrong-arch build.
#
# It runs in a second container, not the build one and not the host, the same
# separation mica-podman:build.sh and pkgs/mica-deploy/build.sh draw: the build
# asserts what it built, this asserts what landed in the directory
# rootfs/build.sh is about to copy from, so an export that dropped a file
# or a mount that wrote somewhere unexpected is caught. On the host it would
# make `file` a host requirement, and docker is the only one this script has;
# the `file` it uses is mica-build-base's, whose version images.env pins a floor
# for.
docker run --rm \
    --platform "linux/${IMAGE_ARCH}" \
    -v "${WORKSPACE}/target:/target:ro" \
    -e "TARGET=${TARGET}" -e "ELF_ARCH=${ELF_ARCH}" \
    --entrypoint /bin/bash "${IMAGE}" -c '
        set -euo pipefail
        for name in micad mica-apid mica-mqttd mica-mqtt-broker; do
            bin="/target/${TARGET}/release/${name}"
            [ -f "${bin}" ] || { echo "error: ${name} was not produced by the build" >&2; exit 1; }
            got="$(file -b "${bin}")"
            case "${got}" in
            *"ELF 64-bit"*"${ELF_ARCH}"*) ;;
            *) echo "error: ${name} is not an ${ELF_ARCH} ELF: ${got}" >&2; exit 1 ;;
            esac
        done
        echo "micad: four ${ELF_ARCH} ELFs in target/${TARGET}/release"
    '
for name in micad mica-apid mica-mqttd mica-mqtt-broker; do
    echo "${WORKSPACE}/target/${TARGET}/release/${name}"
done

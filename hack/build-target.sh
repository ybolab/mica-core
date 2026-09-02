#!/usr/bin/env bash
# Cross-build mosd, apid, mos-mqttd and mos-mqtt-broker for one Rust target
# and verify the ELF.
#
#   bash pkgs/mosd/hack/build-target.sh <rust-target> <elf-arch-substring>
#   bash pkgs/mosd/hack/build-target.sh aarch64-unknown-linux-gnu aarch64
#   bash pkgs/mosd/hack/build-target.sh x86_64-unknown-linux-gnu  x86-64
#
# The compiler is localhost/mos-build-rust's, not the host's, so docker is the
# one thing the host needs: no rustup, no cross linker, no target std. What
# compiled these binaries is a value build-env/images.env records.
set -euo pipefail
cd "$(dirname "$0")/.."
WORKSPACE="$(pwd)"
REPO_ROOT="$(cd "${WORKSPACE}/../.." && pwd)"
FROM_SH="${REPO_ROOT}/build-env/from.sh"

TARGET="${1:?usage: build-target.sh <rust-target> <elf-arch>}"
ELF_ARCH="${2:?usage: build-target.sh <rust-target> <elf-arch>}"

# rootfs/build.sh, pkgs/mosd/hack/build-aarch64.sh and a hand invocation all
# reach this file, and a relative path resolves against whichever is the caller,
# so both roots are derived from $0.
for p in "${WORKSPACE}/Cargo.toml" "${FROM_SH}"; do
    [ -e "${p}" ] || {
        echo "error: ${p} does not exist. pkgs/mosd/hack/build-target.sh derives the workspace as its own directory's parent and the repository as three levels above that; if this file moved, that arithmetic moved with it" >&2
        exit 1
    }
done

command -v docker >/dev/null 2>&1 || {
    echo "error: docker is required and not on PATH. This build runs inside localhost/mos-build-rust rather than on the host's cargo, which is what makes the compiler a value recorded in build-env/images.env instead of whatever the machine happens to have" >&2
    exit 1
}

# The image architecture is derived from the Rust target rather than taken as a
# third argument: a second way to say the same thing can disagree with itself,
# and the disagreement would look like a wrong-arch binary two builds later.
case "${TARGET}" in
aarch64-*) IMAGE_ARCH=amd64 ;; # cross-built FROM an amd64 builder
x86_64-*) IMAGE_ARCH=amd64 ;;
*)
    echo "error: '${TARGET}' is not a target build-env/images.env pins a Rust std for. mos-build-rust ships std for exactly two triples -- RUST_TRIPLE_AMD64 and RUST_TRIPLE_ARM64 -- and adding a third is an images.env edit (RUST_STD_SHA256_<arch>) and not an argument to this script" >&2
    exit 1
    ;;
esac

# The image, out of images.env, and refused by name if it is missing or is the
# wrong architecture -- docker reports the first as a failed pull from a
# registry called `localhost` and the second as a manifest error, neither of
# which names `make build-env`.
mapfile -t FROM_ARGS < <("${FROM_SH}" --arch="${IMAGE_ARCH}" MOS_BUILD_RUST=LOCAL_MOS_BUILD_RUST)
[ "${#FROM_ARGS[@]}" -eq 2 ] || {
    echo "error: build-env/from.sh did not yield localhost/mos-build-rust (see its message above)" >&2
    exit 1
}
IMAGE="${FROM_ARGS[1]#MOS_BUILD_RUST=}"

# The frontend is a separate producer. Finish it on the host (or in the pinned
# Bun container) before Cargo enters the Rust-only cross-build image.
APID_UI_DIST="${WORKSPACE}/apid/ui/dist"
bash "${WORKSPACE}/apid/ui/build.sh" --container --out-dir "${APID_UI_DIST}"

# The cargo caches are repo-local bind mounts, not docker volumes and not
# $HOME/.cargo: `make clean` and `rm -rf _out` then mean what they say, and the
# host carries no Rust state at all. CARGO_HOME=/usr/local/cargo is the image's
# own setting, the same path pkgs/podman/Dockerfile's cargo cache mounts use, so
# the two Rust builds here warm the same directory layout without sharing the
# cache itself.
CARGO_CACHE="${REPO_ROOT}/_out/cargo"
mkdir -p "${CARGO_CACHE}/registry" "${CARGO_CACHE}/git"

# The commit the binaries report is resolved on the host and handed in as an
# environment variable. mosd and apid answer `--version` with `<name> <crate
# version> (<commit>)`, and the commit half cannot be discovered from inside the
# container: this checkout is a git worktree whose `.git` is a file naming a
# gitdir outside the mount, so git in the container reports `not a git
# repository` even though git is installed there. An already-resolved
# MOS_BUILD_COMMIT in the environment wins, so a release pipeline handed a commit
# or a rebuild of an exported tarball with no .git says so rather than being told
# it is `unknown`. An empty value is not an error: `-e MOS_BUILD_COMMIT=` sets
# the empty string, `option_env!` yields `Some("")`, and both crates report
# `unknown` and still exit 0. Dirty is marked, never passed off as the clean SHA,
# and the test is `git status --porcelain` rather than `git diff`: an
# untracked-but-not-ignored `.rs` file is compiled in exactly like a modified
# one.
if [ -z "${MOS_BUILD_COMMIT:-}" ]; then
    MOS_BUILD_COMMIT=""
    if command -v git >/dev/null 2>&1 &&
        git -C "${REPO_ROOT}" rev-parse --git-dir >/dev/null 2>&1; then
        MOS_BUILD_COMMIT="$(git -C "${REPO_ROOT}" rev-parse --short=12 HEAD 2>/dev/null || true)"
        if [ -n "${MOS_BUILD_COMMIT}" ] &&
            [ -n "$(git -C "${REPO_ROOT}" status --porcelain 2>/dev/null)" ]; then
            MOS_BUILD_COMMIT="${MOS_BUILD_COMMIT}-dirty"
        fi
    fi
fi
if [ -n "${MOS_BUILD_COMMIT}" ]; then
    echo "mosd: embedding build commit ${MOS_BUILD_COMMIT}"
else
    echo "mosd: no build commit could be resolved; mosd and apid will report unknown" >&2
fi

# The record the smoke runner reads, written beside the build rather than
# inferred from it. `verify/src/smoke.ts` asserts the commit these binaries
# report against the commit this build embedded, and that second value cannot
# come from `git rev-parse HEAD` at run time, which would pass on any freshly
# built tree. rootfs/build.sh copies this into _out/<board>/ beside the
# factory root it goes into.
#
# This copy describes the last build for any target, which is why the runner
# reads the per-board copy instead: an x64 rootfs build followed by
# `bash pkgs/mosd/hack/build-aarch64.sh` leaves this file saying
# target=aarch64-unknown-linux-gnu while _out/x64/ still holds x86_64 binaries.
#
# Tab-separated `key<TAB>value` with `#` comments, the shape
# build/src/stages.ts writes for factory-root.txt, so one reader reads both.
MOSD_BUILD_RECORD="${REPO_ROOT}/_out/mosd-build.txt"
{
    echo "# What pkgs/mosd/hack/build-target.sh built, and the commit it embedded in mosd and apid."
    echo "# Written on every build. rootfs/build.sh copies it into _out/<board>/."
    echo "# An empty commit means none could be resolved; the binaries then report unknown."
    printf 'target\t%s\n' "${TARGET}"
    printf 'elf-arch\t%s\n' "${ELF_ARCH}"
    printf 'commit\t%s\n' "${MOS_BUILD_COMMIT}"
} >"${MOSD_BUILD_RECORD}"

# The repository is mounted, not pkgs/mosd/. That used to be forced: pkgs/mosd/Cargo.toml
# listed one workspace member outside this directory, and mounting pkgs/mosd/ alone
# made cargo fail on a manifest it could not see. The split extracted that
# crate into its own workspace under pkgs/, so no member reaches outside
# pkgs/mosd/ any more and that reason is gone.
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
# next member added outside pkgs/mosd/ fails with a sentence rather than with a
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
        echo "error: pkgs/mosd/Cargo.toml lists the workspace member '${m}', which resolves outside ${REPO_ROOT}. This build mounts the repository into the container and nothing above it, so cargo would report that member as a missing Cargo.toml rather than as a member the container cannot see" >&2
        exit 1
        ;;
    esac
done < <(sed -n 's/^members = \[\(.*\)\]/\1/p' "${WORKSPACE}/Cargo.toml" | tr ',' '\n' | tr -d ' "')

# `--network host` is not used and is not needed: cargo fetches through the
# container's default network, and the only thing bound in is this repository.
docker run --rm \
    --platform "linux/${IMAGE_ARCH}" \
    -v "${REPO_ROOT}:/src" \
    -v "${CARGO_CACHE}/registry:/usr/local/cargo/registry" \
    -v "${CARGO_CACHE}/git:/usr/local/cargo/git" \
    -w /src/pkgs/mosd \
    -e "TARGET=${TARGET}" \
    -e "MOS_BUILD_COMMIT=${MOS_BUILD_COMMIT}" \
    -e "MOS_APID_UI_DIST_DIR=/src/pkgs/mosd/apid/ui/dist" \
    --entrypoint /bin/bash \
    "${IMAGE}" -c '
        set -euo pipefail
        # The image records what it is; this reads it back and prints it, so the
        # build log answers "which rustc compiled this" without anyone having to
        # know which image was current. An image with no record is one that
        # cannot answer that, and build-env/build.sh refuses to tag one.
        [ -f /etc/mos-build/rust.env ] || {
            echo "error: this image carries no /etc/mos-build/rust.env, so what compiled these binaries cannot be read back out of it" >&2
            exit 1
        }
        . /etc/mos-build/rust.env
        echo "mosd: building ${TARGET} with rustc ${MOS_BUILD_RUSTC} from ${MOS_BUILD_IMAGE} (RUST_SHA256=${MOS_BUILD_RUST_SHA256})"
        # --locked makes Cargo.lock the decision and refuses a build that
        # would quietly update it.
        cargo build --release --locked --target "${TARGET}" \
            -p mosd -p apid -p mos-mqttd -p mos-mqtt-broker
    '

# The ELF check is per binary, not just the first: a target that silently
# produced a host-arch artifact for one crate would otherwise ship and fail at
# exec time on the device, one binary later than a wholly wrong-arch build.
#
# It runs in a second container, not the build one and not the host, the same
# separation pkgs/podman/build.sh and pkgs/rauc/build.sh draw: the build
# asserts what it built, this asserts what landed in the directory
# rootfs/build.sh is about to copy from, so an export that dropped a file
# or a mount that wrote somewhere unexpected is caught. On the host it would
# make `file` a host requirement, and docker is the only one this script has;
# the `file` it uses is mos-build-base's, whose version images.env pins a floor
# for.
docker run --rm \
    --platform "linux/${IMAGE_ARCH}" \
    -v "${WORKSPACE}/target:/target:ro" \
    -e "TARGET=${TARGET}" -e "ELF_ARCH=${ELF_ARCH}" \
    --entrypoint /bin/bash "${IMAGE}" -c '
        set -euo pipefail
        for name in mosd apid mos-mqttd mos-mqtt-broker; do
            bin="/target/${TARGET}/release/${name}"
            [ -f "${bin}" ] || { echo "error: ${name} was not produced by the build" >&2; exit 1; }
            got="$(file -b "${bin}")"
            case "${got}" in
            *"ELF 64-bit"*"${ELF_ARCH}"*) ;;
            *) echo "error: ${name} is not an ${ELF_ARCH} ELF: ${got}" >&2; exit 1 ;;
            esac
        done
        echo "mosd: four ${ELF_ARCH} ELFs in target/${TARGET}/release"
    '
for name in mosd apid mos-mqttd mos-mqtt-broker; do
    echo "${WORKSPACE}/target/${TARGET}/release/${name}"
done

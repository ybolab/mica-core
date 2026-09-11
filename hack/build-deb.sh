#!/usr/bin/env bash
# Build only the requested deployment binaries in the pinned Rust builder.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE="$(cd "${HERE}/.." && pwd)"
REPO_ROOT="$(cd "${WORKSPACE}/../.." && pwd)"
FROM_SH="${REPO_ROOT}/build-env/from.sh"
for p in "${WORKSPACE}/Cargo.toml" "${FROM_SH}"; do
    [ -e "${p}" ] || {
        echo "error: ${p} does not exist. pkgs/mos-deploy/hack/build-deb.sh derives the workspace as its own directory's parent and the repository as three levels above that; if this file moved, that arithmetic moved with it" >&2
        exit 1
    }
done

command -v docker >/dev/null 2>&1 || {
    echo "error: docker is required and not on PATH. The compile runs in a container -- the host carries no cargo -- which is what makes the compiler a value build-env/images.env records" >&2
    exit 1
}

PRODUCER=""
BINS=""
ARCH=""
STAGE=""
while [ "$#" -gt 0 ]; do
    case "$1" in
    --producer)
        PRODUCER="${2-}"
        [ -n "${PRODUCER}" ] || { echo "error: --producer takes a producer name" >&2; exit 1; }
        shift 2
        ;;
    --bins)
        BINS="${2-}"
        [ -n "${BINS}" ] || { echo "error: --bins takes a space-separated binary list" >&2; exit 1; }
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
        echo "usage: bash pkgs/mos-deploy/hack/build-deb.sh --producer <name> --bins \"<binary> ...\" --arch <amd64|arm64> --stage <dir>" >&2
        exit 1
        ;;
    esac
done
[ -n "${PRODUCER}" ] || { echo "error: --producer is required; it names the producer-private target directory, and two producers sharing one would let a build satisfy the independence assertion below with binaries nobody asked for" >&2; exit 1; }
[ -n "${BINS}" ] || { echo "error: --bins is required; there is no default binary list, because a build that picked one would compile a subset nobody asked for" >&2; exit 1; }
[ -n "${ARCH}" ] || { echo "error: --arch is required; guessing the host's would silently produce amd64 binaries for a cx3576 image" >&2; exit 1; }
[ -n "${STAGE}" ] || { echo "error: --stage is required; it is where build-env/deb/build.sh looks for what this hook produced" >&2; exit 1; }
[ -d "${STAGE}" ] || { echo "error: --stage ${STAGE} is not a directory. build-env/deb/build.sh creates it before running this hook" >&2; exit 1; }

BINARIES=()
for b in ${BINS}; do BINARIES+=("${b}"); done

# Every binary this crate declares, through cargo's auto-discovery: `mos-deploy`
# from src/main.rs, the other two from their own filenames under src/bin/. A
# producer names the ones it OWNS in its prepare.sh, and the complement is what
# the independence assertion looks for -- so a fourth binary added to this crate
# is checked without any producer being edited.
ALL_BINARIES=(mos-init mos-shutdown mos-deploy)

# A binary this crate does not declare would make the complement below wrong in
# the direction that matters: it would be treated as owned, and therefore never
# looked for.
for name in "${BINARIES[@]}"; do
    known=0
    for a in "${ALL_BINARIES[@]}"; do
        [ "${name}" != "${a}" ] || known=1
    done
    [ "${known}" = 1 ] || {
        echo "error: '${name}' is not one of this crate's binaries (${ALL_BINARIES[*]}). The independence assertion below checks the COMPLEMENT of what a producer owns, so an unknown name here would silently shrink what is checked" >&2
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

# Producer-private, so this producer has its own cache key and its own output
# directory: sharing pkgs/mos-deploy/target/ with an interactive `cargo build`
# would let a three-binary build satisfy the independence assertion below with
# the signing tool this producer must not ship. Gitignored through
# pkgs/mos-deploy/.gitignore.
TARGET_DIR="${WORKSPACE}/target-deb/${PRODUCER}"
# Shutdown alone uses target-scoped CRT flags and a separate artifact cache.
release_binary() {
    if [ "$1" = mos-shutdown ]; then
        printf '%s/shutdown-static/%s/release/%s\n' "$TARGET_DIR" "$TRIPLE" "$1"
    else
        printf '%s/%s/release/%s\n' "$TARGET_DIR" "$TRIPLE" "$1"
    fi
}

CARGO_CACHE="${REPO_ROOT}/_out/cargo"
mkdir -p "${CARGO_CACHE}/registry" "${CARGO_CACHE}/git"

mapfile -t RUST_FROM < <(bash "${FROM_SH}" --arch=amd64 MOS_BUILD_RUST=LOCAL_MOS_BUILD_RUST)
[ "${#RUST_FROM[@]}" -eq 2 ] || {
    echo "error: build-env/from.sh did not yield localhost/mos-build-rust:amd64 (see its message above); it is built by \`make build-env\`, which must run before any component build that stands on it" >&2
    exit 1
}
RUST_IMAGE="${RUST_FROM[1]#MOS_BUILD_RUST=}"

# The repository at the fixed path /src, not where it happens to live, for the
# reason pkgs/mosd/hack/build-target.sh gives: rustc records the paths it is
# given, so mounting the checkout at its own path would make the binaries depend
# on the directory the repository was cloned into.
# mos-build-side: container-block -- the compiler is localhost/mos-build-rust's,
# recorded in that image's /etc/mos-build/rust.env, and this block refuses an image that
# carries no such record
docker run --rm \
    --label ai-agent=true --network traefik --name "ai-agent-mos-deploy-${PRODUCER}-$$" \
    --platform linux/amd64 --cpus 4 --memory 10g --memory-swap 10g \
    -v "${REPO_ROOT}:/src" \
    -v "${CARGO_CACHE}/registry:/usr/local/cargo/registry" \
    -v "${CARGO_CACHE}/git:/usr/local/cargo/git" \
    -w /src/pkgs/mos-deploy \
    -e "TARGET=${TRIPLE}" \
    -e "ELF_ARCH=${ELF_ARCH}" \
    -e "BINS=${BINARIES[*]}" -e CARGO_BUILD_JOBS=4 \
    -e "CARGO_TARGET_DIR=/src/pkgs/mos-deploy/target-deb/${PRODUCER}" \
    --entrypoint /bin/bash \
    "${RUST_IMAGE}" -c '
        set -euo pipefail
        [ -f /etc/mos-build/rust.env ] || {
            echo "error: this image carries no /etc/mos-build/rust.env, so what compiled these binaries cannot be read back out of it" >&2
            exit 1
        }
        . /etc/mos-build/rust.env
        echo "build-deb: compiling ${BINS} for ${TARGET} with rustc ${MOS_BUILD_RUSTC} from ${MOS_BUILD_IMAGE}"
        # --bin per binary and nothing else: the whole point of a producer is
        # that it cannot emit a binary it does not own. --locked makes
        # Cargo.lock the decision and refuses a build that would quietly update
        # it.
        bins=()
        for b in ${BINS}; do
            if [ "$b" != mos-shutdown ]; then bins+=(--bin "$b"); fi
        done
        if [ "${#bins[@]}" -gt 0 ]; then
            cargo build --release --locked --target "${TARGET}" "${bins[@]}"
        fi
        for name in ${BINS}; do
            if [ "$name" = mos-shutdown ]; then
                # Target-specific configuration leaves host build scripts and
                # every other binary on their original dynamic GNU route.
                env -u RUSTFLAGS -u CARGO_ENCODED_RUSTFLAGS \
                    CARGO_TARGET_DIR="${CARGO_TARGET_DIR}/shutdown-static" \
                    cargo build --release --locked --target "${TARGET}" --bin mos-shutdown \
                    --config "target.${TARGET}.rustflags=[\"-C\",\"target-feature=+crt-static\",\"-C\",\"strip=symbols\"]"
                bin="${CARGO_TARGET_DIR}/shutdown-static/${TARGET}/release/${name}"
                # A flag is not proof. GNU x64 static PIE has a dynamic relocation
                # section, which is permitted only without DT_NEEDED/PT_INTERP.
                readelf -W -l "$bin" > "${bin}.program-headers"
                readelf -W -d "$bin" > "${bin}.dynamic"
                if grep -Ec "^[[:space:]]*INTERP[[:space:]]" "${bin}.program-headers" >/dev/null \
                    || grep -Ec "\(NEEDED\)|\(RPATH\)|\(RUNPATH\)" "${bin}.dynamic" >/dev/null; then
                    echo "error: mos-shutdown is not a standalone static ELF" >&2; exit 1
                fi
                sha256sum "$bin" Cargo.lock
            else
                bin="${CARGO_TARGET_DIR}/${TARGET}/release/${name}"
            fi
            [ -f "${bin}" ] || { echo "error: ${name} was not produced by the build" >&2; exit 1; }
            got="$(file -b "${bin}")"
            case "${got}" in
            *"ELF 64-bit"*"${ELF_ARCH}"*) ;;
            *) echo "error: ${name} is not an ${ELF_ARCH} ELF: ${got}" >&2; exit 1 ;;
            esac
        done
    '
# mos-build-side: host

# What this producer OWNS, checked before what it must not hold. Without this the
# scan below would pass over a directory the build never wrote -- an "is absent"
# assertion over an empty tree reports green forever.
for name in "${BINARIES[@]}"; do
    [ -f "$(release_binary "$name")" ] || {
        echo "error: $(release_binary "$name") does not exist after the build. The compile reported success, so this is the export or the target directory and not the compiler" >&2
        exit 1
    }
done

# INDEPENDENCE, asserted rather than described. `cargo build --bin mos-deploy
# --bin mos-init` is the intent; this is the evidence, and it is what a later
# gate can point at. A binary here means the producer boundary leaked -- a stale
# target directory reused, or a --bin list that grew -- and the one binary this
# complement holds is the offline signing tool, which no device may carry.
stray=""
for name in "${EXCLUDED[@]}"; do
    while IFS= read -r f; do
        [ -z "${f}" ] || stray="${stray} ${f}"
    done < <(find "${TARGET_DIR}" -type f -name "${name}")
done
if [ -n "${stray}" ]; then
    echo "error: the ${PRODUCER} producer's target directory holds binaries it does not own:${stray}. This producer ships the DEVICE side of pkgs/mos-deploy and nothing else; mos-deploy is the release-host signing tool. See pkgs/mos-deploy/README.md" >&2
    exit 1
fi
echo "build-deb: ${TARGET_DIR} holds ${BINARIES[*]} and none of ${EXCLUDED[*]}"

# The staging directory rather than the release directory itself: a cargo target
# directory is gigabytes of intermediates, and buildx would walk all of it to
# find two files.
for name in "${BINARIES[@]}"; do
    cp "$(release_binary "$name")" "${STAGE}/${name}"
done
echo "build-deb: staged ${BINARIES[*]} into ${STAGE}"

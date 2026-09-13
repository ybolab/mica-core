#!/usr/bin/env bash
# EVERY input `make os-debs` needs and does not have, reported in ONE run,
# before any container is started.
#
#   bash build-env/deb/preflight.sh
#   bash build-env/deb/preflight.sh --producer board-cx3576
#
# It reports missing inputs; it never produces them. Producers come from
# producers.sh, images from from.sh, and artefacts from each PREPARE hook in
# check-only mode, so the checks are the build's own. BOARD_DIR reaches hooks
# as it does in `make os-debs`.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${HERE}/../.." && pwd)"
FROM_SH="${REPO_ROOT}/build-env/from.sh"
PRODUCERS_SH="${HERE}/producers.sh"
for p in "${REPO_ROOT}/Makefile" "${FROM_SH}" "${PRODUCERS_SH}"; do
    [ -e "${p}" ] || {
        echo "error: ${p} does not exist. build-env/deb/preflight.sh derives the repository as two levels above itself; if this file moved, that arithmetic moved with it" >&2
        exit 1
    }
done

ONLY=""
while [ "$#" -gt 0 ]; do
    case "$1" in
    --producer)
        ONLY="${2-}"
        [ -n "${ONLY}" ] || { echo "error: --producer takes a producer name" >&2; exit 1; }
        shift 2
        ;;
    *)
        echo "usage: bash build-env/deb/preflight.sh [--producer <name>]" >&2
        exit 1
        ;;
    esac
done
[ -z "${ONLY}" ] || bash "${PRODUCERS_SH}" --dir-for "${ONLY}" >/dev/null

# An `all` producer packs at the host's architecture.
case "$(uname -m)" in
x86_64) HOST_ARCH=amd64 ;;
aarch64 | arm64) HOST_ARCH=arm64 ;;
*)
    echo "error: $(uname -m) is not an architecture build-env/images.env builds a mica-build-deb for, so there is no container any producer here could pack in" >&2
    exit 1
    ;;
esac

# Captured, not piped into the loop, so a discovery failure is not swallowed.
ROWS="$(bash "${PRODUCERS_SH}")"

PRODUCERS=0
CTX_N=0
HOOK_N=0
IMAGE_N=0
ARTEFACT_N=0
VF_N=0
REPORTS=()
MISSING_N=0
WARNED_N=0
# Reports and counts are separate: a hook reports several inputs in one block.
# MISSING: nothing in the run produces it, so the run is refused.
# WARNED: the producer builds it itself at a cost; reported, not refused.
note_missing() {
    REPORTS+=("$1")
    MISSING_N=$((MISSING_N + 1))
}

note_warning() {
    REPORTS+=("$1")
    WARNED_N=$((WARNED_N + 1))
}
# Each (key, architecture) is checked once.
declare -A IMAGE_SEEN=()

while read -r producer dir arches _packages _enablement; do
    [ -n "${producer}" ] || continue
    [ -z "${ONLY}" ] || [ "${producer}" = "${ONLY}" ] || continue
    PRODUCERS=$((PRODUCERS + 1))
    producer_dir="${REPO_ROOT}/${dir}"

    # Sourced in a subshell so one producer cannot affect the next; a matrix
    # producer's instance file first, so the declaration can name it.
    instance_env="$(bash "${PRODUCERS_SH}" --instance-for "${producer}")"
    vals="$(
        BUILD_CONTEXTS=""
        FROM_IMAGES=""
        PREPARE=""
        PREFLIGHT=""
        VERSION_FROM=""
        # shellcheck disable=SC1090
        [ -z "${instance_env}" ] || . "${REPO_ROOT}/${instance_env}"
        # shellcheck disable=SC1090
        . "${producer_dir}/producer.env"
        printf 'C=%s\nF=%s\nP=%s\nL=%s\nV=%s\n' "${BUILD_CONTEXTS}" "${FROM_IMAGES}" "${PREPARE}" "${PREFLIGHT}" "${VERSION_FROM}"
    )"
    contexts="$(printf '%s\n' "${vals}" | sed -n 's/^C=//p')"
    from_images="$(printf '%s\n' "${vals}" | sed -n 's/^F=//p')"
    prepare="$(printf '%s\n' "${vals}" | sed -n 's/^P=//p')"
    preflight="$(printf '%s\n' "${vals}" | sed -n 's/^L=//p')"
    version_from="$(printf '%s\n' "${vals}" | sed -n 's/^V=//p')"

    # Build contexts (build.sh checks these too, for single-producer builds).
    for entry in ${contexts}; do
        CTX_N=$((CTX_N + 1))
        name="${entry%%=*}"
        path="${entry#*=}"
        if [ -z "${name}" ] || [ -z "${path}" ] || [ "${name}" = "${entry}" ]; then
            note_missing "error: ${dir}/producer.env declares the build context '${entry}', which is not <context name>=<repository-relative path>."
            continue
        fi
        [ -e "${REPO_ROOT}/${path}" ] || note_missing "error: ${dir}/producer.env declares the build context '${name}=${path}' and ${path} does not exist.
Every build context a producer names is a COMMITTED tree, so this is a path that
moved or a checkout that is incomplete -- not something a build produces."
    done

    # The upstream version source; its value's shape is left to build.sh.
    if [ -n "${version_from}" ]; then
        VF_N=$((VF_N + 1))
        vf_path="${version_from%%:*}"
        vf_key="${version_from##*:}"
        if [ -z "${vf_path}" ] || [ -z "${vf_key}" ] || [ "${vf_path}" = "${version_from}" ]; then
            note_missing "error: ${dir}/producer.env declares VERSION_FROM='${version_from}', which is not <repository-relative env file>:<KEY>."
        elif [ ! -f "${REPO_ROOT}/${vf_path}" ]; then
            note_missing "error: ${dir}/producer.env declares VERSION_FROM=${version_from} and ${vf_path} does not exist.
The upstream version this producer stamps comes from that file or from nowhere."
        elif [ -z "$(sed -n "s/^${vf_key}=//p" "${REPO_ROOT}/${vf_path}" | head -n1)" ]; then
            note_missing "error: ${dir}/producer.env declares VERSION_FROM=${version_from} and ${vf_path} declares no non-empty ${vf_key}.
An empty upstream version would compose into '+git<commit>-1', which dpkg accepts and orders below every real version."
        fi
    fi

    # The hook file.
    if [ -n "${prepare}" ]; then
        HOOK_N=$((HOOK_N + 1))
        [ -f "${producer_dir}/${prepare}" ] || note_missing "error: ${dir}/producer.env names PREPARE=${prepare} and ${dir}/${prepare} does not exist.
The hook is the producer's own half of its build: it is what produces the payload
the packing step copies, so without it the build stages nothing."
    fi

    # The base images; every producer packs in mica-build-deb, so it is always added.
    keys="LOCAL_MICA_BUILD_DEB"
    for entry in ${from_images}; do
        keys="${keys} ${entry#*=}"
    done
    for arch in $(printf '%s' "${arches}" | tr ',' ' '); do
        image_arch="${arch}"
        [ "${arch}" != all ] || image_arch="${HOST_ARCH}"
        for key in ${keys}; do
            [ -z "${IMAGE_SEEN[${key}/${image_arch}]:-}" ] || continue
            IMAGE_SEEN["${key}/${image_arch}"]=1
            IMAGE_N=$((IMAGE_N + 1))
            out=""
            rc=0
            out="$(bash "${FROM_SH}" --arch="${image_arch}" --ref "${key}" 2>&1)" || rc=$?
            [ "${rc}" -eq 0 ] || note_missing "${out}"
        done
    done

    # The producer's own artefacts, via its PREPARE hook in check-only mode
    # (opt-in with PREFLIGHT, since an untaught hook would do its full build).
    # The hook gets MICA_DEB_REPO_ROOT, MICA_DEB_PRODUCER, MICA_DEB_PRODUCER_DIR,
    # MICA_DEB_ARCH and MICA_DEB_PREFLIGHT=1 (no MICA_DEB_STAGE), must print
    # `preflight-examined:`, `preflight-missing:` and `preflight-warned:` counts
    # on every path, and exits non-zero only when missing is not zero.
    if [ -n "${preflight}" ] && [ "${preflight}" != 0 ]; then
        [ -n "${prepare}" ] || {
            echo "error: ${dir}/producer.env declares PREFLIGHT=${preflight} and no PREPARE. The pre-flight mode is a mode OF the PREPARE hook; there is no other script here to run in it" >&2
            exit 1
        }
        # An absent hook was already reported above and is not run.
        for arch in $(! [ -f "${producer_dir}/${prepare}" ] || printf '%s' "${arches}" | tr ',' ' '); do
            out=""
            rc=0
            out="$(
                inst="$(bash "${PRODUCERS_SH}" --instance-for "${producer}")"
                MICA_DEB_PREFLIGHT=1 \
                    MICA_DEB_REPO_ROOT="${REPO_ROOT}" \
                    MICA_DEB_PRODUCER="${producer}" \
                    MICA_DEB_PRODUCER_DIR="${producer_dir}" \
                    MICA_DEB_INSTANCE="${producer#*@}" \
                    MICA_DEB_INSTANCE_ENV="${inst:+${REPO_ROOT}/${inst}}" \
                    MICA_DEB_ARCH="${arch}" \
                    bash "${producer_dir}/${prepare}" 2>&1
            )" || rc=$?
            n="$(printf '%s\n' "${out}" | sed -n 's/^preflight-examined: //p' | tail -1)"
            m="$(printf '%s\n' "${out}" | sed -n 's/^preflight-missing: //p' | tail -1)"
            w="$(printf '%s\n' "${out}" | sed -n 's/^preflight-warned: //p' | tail -1)"
            # Each count checked on its own; only examined may not be zero.
            bad=""
            case "${n}" in '' | *[!0-9]* | 0) bad="preflight-examined" ;; esac
            case "${m}" in '' | *[!0-9]*) bad="${bad:+${bad} and }preflight-missing" ;; esac
            case "${w}" in '' | *[!0-9]*) bad="${bad:+${bad} and }preflight-warned" ;; esac
            [ -z "${bad}" ] || {
                echo "error: ${dir}/${prepare} ran in pre-flight mode for ${arch} and did not print a usable ${bad} count. A hook says what it looked at with 'preflight-examined: <count>', how much of it nothing in the run can make with 'preflight-missing: <count>', and how much the producer will make for itself with 'preflight-warned: <count>' -- all three on every path, and the first above zero. Without them a hook that checked nothing reads exactly like one that checked everything, and a report naming four files is counted as one:" >&2
                printf '%s\n' "${out}" >&2
                exit 1
            }
            ARTEFACT_N=$((ARTEFACT_N + n))
            if [ "${m}" -gt 0 ] || [ "${w}" -gt 0 ]; then
                # The count lines are for this script, not the operator.
                REPORTS+=("$(printf '%s\n' "${out}" | grep -v '^preflight-\(examined\|missing\|warned\): ' || true)")
                MISSING_N=$((MISSING_N + m))
                WARNED_N=$((WARNED_N + w))
            fi
            # A non-zero exit with nothing missing is a failure of the hook itself.
            [ "${rc}" -eq 0 ] || [ "${m}" -gt 0 ] || {
                echo "error: ${dir}/${prepare} exited ${rc} in pre-flight mode for ${arch} while reporting nothing missing. A hook refuses by counting what it cannot find; a non-zero exit with a zero missing count is a failure of the hook itself:" >&2
                printf '%s\n' "${out}" >&2
                exit 1
            }
        done
    fi
done <<<"${ROWS}"

EXAMINED=$((CTX_N + HOOK_N + IMAGE_N + ARTEFACT_N + VF_N))
BREAKDOWN="${CTX_N} build context(s), ${HOOK_N} PREPARE hook(s), ${IMAGE_N} base image(s), ${VF_N} upstream version source(s) and ${ARTEFACT_N} producer artefact(s)"

# Examining nothing is refused rather than reported green -- unless the tree
# declares no producer at all and imports everything through deps/packages,
# where there is no input of a build to examine.
if [ "${EXAMINED}" -eq 0 ] && [ "${PRODUCERS}" -eq 0 ]; then
    pins="$(find "${REPO_ROOT}/deps/packages" -maxdepth 1 -name '*.json' 2>/dev/null | wc -l)"
    [ "${pins}" -gt 0 ] || {
        echo "error: the pre-flight found no producer and no package pin under deps/packages; there is nothing this repository builds or imports" >&2
        exit 1
    }
    echo "preflight: no producer under ${REPO_ROOT}; ${pins} package pin(s) under deps/packages import what it composes from, and there is no build input to examine"
    exit 0
fi
[ "${EXAMINED}" -gt 0 ] || {
    echo "error: the pre-flight examined 0 inputs across ${PRODUCERS} producer(s) (${BREAKDOWN}) and would report success by having checked nothing. Every one of those counts is read out of the tree at run time; a zero means the declarations moved, not that there is nothing to build" >&2
    exit 1
}

[ "${#REPORTS[@]}" -eq 0 ] || printf '%s\n\n' "${REPORTS[@]}" >&2

# The warning count gets its own line so it stays visible.
PRESENT_N=$((EXAMINED - MISSING_N - WARNED_N))
if [ "${MISSING_N}" -gt 0 ]; then
    echo "preflight: ${MISSING_N} of ${EXAMINED} examined inputs are missing across ${PRODUCERS} producer(s): ${BREAKDOWN}. Every one of them is listed above -- nothing was built and no container was started." >&2
else
    echo "preflight: ${PRESENT_N} of ${EXAMINED} examined inputs are present across ${PRODUCERS} producer(s): ${BREAKDOWN}"
fi
if [ "${WARNED_N}" -gt 0 ]; then
    echo "preflight: a further ${WARNED_N} of ${EXAMINED} are absent and will be BUILT BY THE RUN ITSELF, at the cost named in the warnings above. Making them first is how that cost is paid where it can be seen; it is not a prerequisite." >&2
fi
[ "${MISSING_N}" -eq 0 ] || exit 1

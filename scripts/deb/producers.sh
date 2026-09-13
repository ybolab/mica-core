#!/usr/bin/env bash
# Every Debian package producer in this repository, discovered from the tree.
#
#   bash build-env/deb/producers.sh
#   -> micad pkgs/micad/deb/micad amd64,arm64 micad,mica-apid micad=1,mica-apid=1
#      mqtt pkgs/micad/deb/mqtt amd64,arm64 mica-mqttd,mica-mqtt-broker mica-mqttd=0,mica-mqtt-broker=0
#
#   bash build-env/deb/producers.sh --dir-for <producer>
#   -> pkgs/micad/deb/micad
#
#   bash build-env/deb/producers.sh --instance-for <producer>@<instance>
#   -> boards/cx3576/board.env      (the FOR_EACH file the instance is; empty for a plain producer)
#
#   bash build-env/deb/producers.sh --control-for <producer>
#   -> boards/cx3576/package/control   (CONTROL_DIR of the producer.env, else <dir>/control)
#
# Five space-separated fields, sorted by producer name:
#
#   <producer>    the producer directory's basename, and what `make os-deb-<x>`
#                 selects on; a matrix producer (FOR_EACH in its producer.env)
#                 is one row per instance, <basename>@<instance>
#   <dir>         the producer directory, repository-relative
#   <arches>      comma-separated, from ARCHES
#   <packages>    comma-separated, from PACKAGES
#   <enablement>  comma-separated <package>=<count>, from ENABLEMENT, or `-`
#
# A producer is a directory anywhere in the tree holding both a Dockerfile and a
# producer.env. This is the only discovery; every other script reads it. The
# convention is documented in build-env/deb/README.md.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${HERE}/../.." && pwd)"
[ -e "${REPO_ROOT}/Makefile" ] || {
    echo "error: ${REPO_ROOT}/Makefile does not exist. build-env/deb/producers.sh derives the repository as two levels above itself; if this file moved, that arithmetic moved with it" >&2
    exit 1
}

DIR_FOR=""
INSTANCE_FOR=""
CONTROL_FOR=""
while [ "$#" -gt 0 ]; do
    case "$1" in
    --dir-for)
        DIR_FOR="${2-}"
        [ -n "${DIR_FOR}" ] || { echo "error: --dir-for takes a producer name" >&2; exit 1; }
        shift 2
        ;;
    --instance-for)
        INSTANCE_FOR="${2-}"
        [ -n "${INSTANCE_FOR}" ] || { echo "error: --instance-for takes a producer name" >&2; exit 1; }
        shift 2
        ;;
    --control-for)
        CONTROL_FOR="${2-}"
        [ -n "${CONTROL_FOR}" ] || { echo "error: --control-for takes a producer name" >&2; exit 1; }
        shift 2
        ;;
    *)
        echo "usage: bash build-env/deb/producers.sh [--dir-for <producer> | --instance-for <producer> | --control-for <producer>]" >&2
        exit 1
        ;;
    esac
done

# Build outputs may hold staged copies of a producer.env.
mapfile -t ENVS < <(
    find "${REPO_ROOT}" \
        \( -path "${REPO_ROOT}/.git" -o -path "${REPO_ROOT}/_out" -o -path "${REPO_ROOT}/tmp" -o -name node_modules \) -prune -o \
        -type f -name producer.env -print | LC_ALL=C sort
)

ROWS=()
declare -A SEEN=()
declare -A INSTANCE_OF=()
declare -A CONTROL_OF=()
for env_file in ${ENVS[@]+"${ENVS[@]}"}; do
    dir="$(dirname "${env_file}")"
    rel="${dir#"${REPO_ROOT}"/}"
    producer="$(basename "${dir}")"

    [ -f "${dir}/Dockerfile" ] || {
        echo "error: ${rel}/producer.env has no Dockerfile beside it. A producer is the PAIR: producer.env declares what it emits and the Dockerfile stages and packs it. This directory has only the declaration" >&2
        exit 1
    }

    # Readers split the output on spaces.
    case "${producer}" in
    *[[:space:]]*)
        echo "error: the producer directory ${rel} has a name containing whitespace. build-env/deb/producers.sh emits space-separated fields and every reader splits on that, so such a name would be read as a different producer entirely" >&2
        exit 1
        ;;
    esac

    # A MATRIX PRODUCER runs once per file its FOR_EACH glob matches (a
    # repository-relative glob of plain KEY=value files, boards/*/board.env):
    # the file's assignments are in the environment when producer.env is
    # sourced, so PACKAGES="mica-board-${LAYOUT_BOARD}" names the instance's
    # package. The instance is the matched file's directory name, and the
    # producer's row is <producer>@<instance>. A producer without FOR_EACH
    # is one instance, itself.
    # Read, not sourced: the other values may name the instance's variables.
    for_each="$(sed -n 's/^FOR_EACH=//p' "${env_file}" | head -n1 | sed 's/^"\(.*\)"$/\1/')"
    instances=("")
    if [ -n "${for_each}" ]; then
        instances=()
        for inst in "${REPO_ROOT}"/${for_each}; do
            [ -f "${inst}" ] || continue
            instances+=("${inst}")
        done
        [ "${#instances[@]}" -gt 0 ] || {
            echo "error: ${rel}/producer.env declares FOR_EACH='${for_each}', which matches no file under ${REPO_ROOT}; a matrix producer over nothing would build nothing and report success" >&2
            exit 1
        }
    fi
    for inst in "${instances[@]}"; do
    name="${producer}"
    [ -z "${inst}" ] || name="${producer}@$(basename "$(dirname "${inst}")")"
    [ -z "${SEEN[${name}]:-}" ] || {
        echo "error: two producers are both named '${name}': ${SEEN[${name}]} and ${rel}. The name is the producer's identity -- it is what \`make os-deb-${name}\` selects on -- so one of them has to be renamed" >&2
        exit 1
    }
    SEEN["${name}"]="${rel}"
    INSTANCE_OF["${name}"]="${inst#"${REPO_ROOT}"/}"
    # Where the package control templates are: CONTROL_DIR (repository-relative,
    # named by the instance for a matrix producer), else the producer's control/.
    CONTROL_OF["${name}"]="$(
        # shellcheck disable=SC1090
        [ -z "${inst}" ] || . "${inst}"
        # shellcheck disable=SC1090
        . "${env_file}"
        printf '%s' "${CONTROL_DIR:-${rel}/control}"
    )"
    # Sourced in a subshell so one producer cannot affect the next; the
    # instance file first, so the producer's values can name it.
    vals="$(
        # shellcheck disable=SC1090
        [ -z "${inst}" ] || . "${inst}"
        # shellcheck disable=SC1090
        . "${env_file}"
        printf 'A=%s\nP=%s\nE=%s\n' "${ARCHES-}" "${PACKAGES-}" "${ENABLEMENT-}"
    )"
    arches="$(printf '%s\n' "${vals}" | sed -n 's/^A=//p')"
    packages="$(printf '%s\n' "${vals}" | sed -n 's/^P=//p')"
    enablement="$(printf '%s\n' "${vals}" | sed -n 's/^E=//p')"

    # ENABLEMENT is only carried through; package-gate.sh enforces it.
    [ -n "${packages// /}" ] || {
        echo "error: ${rel}/producer.env declares no PACKAGES. That is the list of Debian packages this producer emits, and everything downstream is derived from it: an empty one builds nothing, clears nothing out of the pool and contributes nothing to any expectation -- which reports green rather than reporting this" >&2
        exit 1
    }
    [ -n "${arches// /}" ] || {
        echo "error: ${rel}/producer.env declares no ARCHES. That is the list of architectures this producer builds: amd64, arm64, or 'all' for an architecture-independent package" >&2
        exit 1
    }
    for a in ${arches}; do
        case "${a}" in
        amd64 | arm64 | all) ;;
        *)
            echo "error: ${rel}/producer.env declares ARCHES entry '${a}'. The only values are amd64, arm64 and all; amd64 and arm64 are what build-env/images.env pins a mica-build-deb for, and 'all' is an architecture-independent package that is a valid member of every pool" >&2
            exit 1
            ;;
        esac
    done
    case " ${arches} " in
    *" all "*)
        [ "$(printf '%s\n' ${arches} | wc -l)" -eq 1 ] || {
            echo "error: ${rel}/producer.env declares ARCHES='${arches}', mixing 'all' with a specific architecture. An 'all' package is already a member of every pool, so the pair says both that this producer is architecture-independent and that it is not" >&2
            exit 1
        }
        ;;
    esac

    ROWS+=("${name} ${rel} $(printf '%s' "${arches}" | tr -s ' ' ',') $(printf '%s' "${packages}" | tr -s ' ' ',') $(if [ -n "${enablement// /}" ]; then printf '%s' "${enablement}" | tr -s ' ' ','; else printf '%s' -; fi)")
    done
done

# An empty discovery would make every caller green having done nothing --
# unless the repository imports what it composes from: deps/packages/*.json
# pinning at least one package is the declaration that this tree builds
# nothing of its own, and its callers then work over the lock's rows.
pins=0
for pin in "${REPO_ROOT}"/deps/packages/*.json; do [ -e "${pin}" ] && pins=$((pins + 1)); done
if [ "${#ROWS[@]}" -eq 0 ] && [ "${pins}" -gt 0 ]; then
    echo "producers.sh: no producer under ${REPO_ROOT}; ${pins} package pin(s) under deps/packages import what this repository composes from" >&2
    exit 0
fi
[ "${#ROWS[@]}" -gt 0 ] || {
    echo "error: no package producer was found anywhere under ${REPO_ROOT}. Every caller of this script would then have an empty set to work over: \`make os-debs\` would build nothing and build-env/deb/package-gate.sh would assert nothing, and both would report success. A producer is a directory holding BOTH a Dockerfile and a producer.env; see build-env/deb/README.md" >&2
    exit 1
}

mapfile -t ROWS < <(printf '%s\n' "${ROWS[@]}" | LC_ALL=C sort)

if [ -n "${CONTROL_FOR}" ]; then
    [ -n "${SEEN[${CONTROL_FOR}]:-}" ] || { echo "error: '${CONTROL_FOR}' is not a producer this repository defines. Discovered: $(printf '%s\n' "${ROWS[@]}" | cut -d' ' -f1 | tr '\n' ' ')" >&2; exit 1; }
    printf '%s\n' "${CONTROL_OF[${CONTROL_FOR}]}"
    exit 0
fi
if [ -n "${INSTANCE_FOR}" ]; then
    [ -n "${SEEN[${INSTANCE_FOR}]:-}" ] || { echo "error: '${INSTANCE_FOR}' is not a producer this repository defines. Discovered: $(printf '%s\n' "${ROWS[@]}" | cut -d' ' -f1 | tr '\n' ' ')" >&2; exit 1; }
    printf '%s\n' "${INSTANCE_OF[${INSTANCE_FOR}]}"
    exit 0
fi
if [ -n "${DIR_FOR}" ]; then
    for row in "${ROWS[@]}"; do
        read -r producer dir _rest <<<"${row}"
        [ "${producer}" = "${DIR_FOR}" ] || continue
        echo "${dir}"
        exit 0
    done
    echo "error: '${DIR_FOR}' is not a producer this repository defines. Discovered: $(printf '%s\n' "${ROWS[@]}" | cut -d' ' -f1 | tr '\n' ' ')-- a producer is a directory holding both a Dockerfile and a producer.env, and its NAME is that directory's basename; see build-env/deb/README.md" >&2
    exit 1
fi

printf '%s\n' "${ROWS[@]}"

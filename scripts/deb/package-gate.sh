#!/usr/bin/env bash
# The package-level gates of PLAN-036 section 6, over the built pools.
#
#   bash scripts/deb/package-gate.sh
#
#   reads   _out/debs/<arch>/pool/*.deb          (built by `make os-debs`)
#   asserts the facts listed below, per architecture
#
# Facts are read out of the archives with dpkg-deb (inside the IMAGE_MICA_BUILD_BASE image);
# expectations come from producers.sh, each producer.env and the lock.
#
#   a  no non-directory path is in two archives of one pool, except packages
#      that every one declare mutual unversioned Conflicts, by name or through
#      a virtual name each provides; Replaces is refused.
#   b  Architecture, the package set (producers plus lock), one git stamp over
#      the archives built here, imported archives equal to their lock rows, and
#      the Depends closure.
#   c  two builds under one SOURCE_DATE_EPOCH are byte-identical.
#   d  every package ships a non-empty /usr/share/doc/<package>/copyright.
#   e  each package ships as many multi-user.target.wants symlinks as its
#      ENABLEMENT row declares.
#   f  no package carries DEBIAN/conffiles (the root is immutable).
#   g  every maintainer script parses as POSIX sh.
#   h  every producer contributed its packages and every archive maps back to a
#      producer or a lock row; PACKAGES matches the control templates.
#   i  `all` archives are byte-identical across pools. A dependency on a local
#      package is pinned exactly when both sides are built here and unversioned
#      across the lock; a Provides-only name is local-virtual; anything else is
#      external and only reported.
#
# Not checked: an APT install into a clean root (external dependencies are the
# composer's).
set -euo pipefail

# Runs over the pool of the repository this substrate is checked out in.
cd "$(dirname "$0")/../.."
REPO_ROOT="$(pwd)"
FROM_SH="${REPO_ROOT}/scripts/build/from.sh"
PRODUCERS_SH="${REPO_ROOT}/scripts/deb/producers.sh"
BUILD_SH="${REPO_ROOT}/scripts/deb/build.sh"
DIST="${REPO_ROOT}/_out/debs"
[ "$#" -eq 0 ] || { echo "usage: bash scripts/deb/package-gate.sh    (no arguments; it reads the pools under _out/debs and the pins under deps/packages/)" >&2; exit 1; }
LOCK_SH="${REPO_ROOT}/scripts/deb/lock.sh"
for p in "${FROM_SH}" "${PRODUCERS_SH}" "${BUILD_SH}" "${LOCK_SH}"; do
    [ -e "${p}" ] || {
        echo "error: ${p} does not exist. This gate derives the repository as two levels above scripts/deb; if this file moved, that arithmetic moved with it" >&2
        exit 1
    }
done

command -v docker >/dev/null 2>&1 || {
    echo "error: docker is required and not on PATH. dpkg-deb runs inside the IMAGE_MICA_BUILD_BASE image rather than on the host, and the reproducibility check drives a real package build" >&2
    exit 1
}

# The producer set, captured rather than piped so a discovery refusal is not swallowed.
mapfile -t ROWS < <(bash "${PRODUCERS_SH}")
# A repository that imports everything declares no producer (producers.sh
# says so and exits 0); the gate then runs over the lock's rows alone.
LOCK_ROW_N="$(bash "${REPO_ROOT}/scripts/deb/lock.sh" --rows | grep -c . || true)"
[ "${#ROWS[@]}" -gt 0 ] || [ "${LOCK_ROW_N}" -gt 0 ] || {
    echo "error: scripts/deb/producers.sh named no producer (see its message above). Every expectation below is derived from that set, and over an empty one they all hold" >&2
    exit 1
}

# The architectures this repository's pool holds: what its producers declare
# (an `all` producer is a member of every pool this repository builds for,
# so alone it means both) and what the lock imports -- read from those
# declarations, never discovered from _out/debs, so a missing pool fails.
declared_arches=" "
for row in ${ROWS[@]+"${ROWS[@]}"}; do
    read -r _p _d arches _rest <<<"${row}"
    for a in $(printf '%s' "${arches}" | tr ',' ' '); do
        case "${a}" in all) declared_arches="${declared_arches}amd64 arm64 " ;; *) declared_arches="${declared_arches}${a} " ;; esac
    done
done
while IFS=$'\t' read -r _n _v larch _rest; do
    [ -n "${larch}" ] || continue
    case "${larch}" in all) declared_arches="${declared_arches}amd64 arm64 " ;; *) declared_arches="${declared_arches}${larch} " ;; esac
done < <(bash "${REPO_ROOT}/scripts/deb/lock.sh" --rows)
ARCHES=()
for a in amd64 arm64; do
    case "${declared_arches}" in *" ${a} "*) ARCHES+=("${a}") ;; esac
done
[ "${#ARCHES[@]}" -gt 0 ] || { echo "error: neither a producer nor a lock row names an architecture, so there is no pool to gate" >&2; exit 1; }
for arch in "${ARCHES[@]}"; do
    [ -d "${DIST}/${arch}/pool" ] || {
        echo "error: ${DIST}/${arch}/pool does not exist, so there is nothing to check for ${arch}. Build it with \`make os-debs\`; a gate that skipped the missing architecture would report on half a pool" >&2
        exit 1
    }
done

case "$(uname -m)" in
x86_64) IMAGE_ARCH=amd64 ;;
aarch64 | arm64) IMAGE_ARCH=arm64 ;;
*)
    echo "error: $(uname -m) is not an architecture the IMAGE_MICA_BUILD_BASE index carries, so there is no container to read the archives in" >&2
    exit 1
    ;;
esac
# The host architecture's image: reading archives needs no emulation.
mapfile -t FROM_ARGS < <(bash "${FROM_SH}" --arch="${IMAGE_ARCH}" MICA_BUILD_DEB=IMAGE_MICA_BUILD_BASE)
[ "${#FROM_ARGS[@]}" -eq 2 ] || {
    echo "error: scripts/build/from.sh did not yield IMAGE_MICA_BUILD_BASE (see its message above); build-env/images.env is the pinned release asset (make deps)" >&2
    exit 1
}
IMAGE="${FROM_ARGS[1]#MICA_BUILD_DEB=}"

WORK="${REPO_ROOT}/tmp/deb-package-gate"
rm -rf "${WORK}"
mkdir -p "${WORK}"

# Discovery rows and control templates, staged so the repository is not mounted.
TMPL="${WORK}/tmpl"
mkdir -p "${TMPL}"
if [ "${#ROWS[@]}" -gt 0 ]; then printf '%s\n' "${ROWS[@]}" >"${TMPL}/producers.tsv"; else : >"${TMPL}/producers.tsv"; fi
for row in "${ROWS[@]}"; do
    read -r producer dir _arches _packages _enablement <<<"${row}"
    mkdir -p "${TMPL}/p/${producer}"
    # The templates are where producers.sh says (CONTROL_DIR, else <dir>/control);
    # a missing directory is reported by the container.
    control="$(bash "${PRODUCERS_SH}" --control-for "${producer}")"
    [ ! -d "${REPO_ROOT}/${control}" ] || cp -R "${REPO_ROOT}/${control}" "${TMPL}/p/${producer}/control"
done

# a, b, d, e, f, g, h, i: one container reading both pools, with the lock rows staged.
bash "${LOCK_SH}" --rows >"${TMPL}/lock.tsv"
STATIC_LOG="${WORK}/static.log"
static_status=0
docker run --rm -i \
    --label ai-agent=true \
    -v "${DIST}:/dist:ro" \
    -v "${TMPL}:/tmpl:ro" \
    --entrypoint /bin/bash \
    "${IMAGE}" -s "${ARCHES[@]}" 2>&1 <<'INNER' | tee "${STATIC_LOG}" || static_status=1
set -euo pipefail

PASS_N=0
FAIL_N=0
pass() { PASS_N=$((PASS_N + 1)); echo "PASS: $1"; }
fail() { FAIL_N=$((FAIL_N + 1)); echo "FAIL: $1"; }

# a -- does $1 name $2 in an unversioned Conflicts, either by its name or by
# a virtual name $2 provides. A package that provides and conflicts with one
# virtual name (`Provides: mica-board`, `Conflicts: mica-board`) excludes every
# other provider without listing them, so a new provider edits no sibling.
conflicts_with() {
    local c
    for c in ${CONFLICTS_WITH[$1]:-}; do
        [ "${c}" != "$2" ] || return 0
        case " ${PROVIDED_BY[${c}]:-} " in *" $2 "*) return 0 ;; esac
    done
    return 1
}
# a -- do $1 and $2 each name the other.
mutually_conflicting() {
    conflicts_with "$1" "$2" && conflicts_with "$2" "$1"
}

ARCHES=("$@")
# The lock rows, keyed <package>|<arch>; an `all` row belongs to every pool.
declare -A LOCK_VERSION=() LOCK_SHA=() LOCK_REPO=() LOCK_COMMIT=()
LOCKED_NAMES=" "
while IFS=$'\t' read -r name version larch digest repo commit; do
    [ -n "${name}" ] || continue
    LOCK_VERSION["${name}|${larch}"]="${version}"; LOCK_SHA["${name}|${larch}"]="${digest}"
    LOCK_REPO["${name}|${larch}"]="${repo}"; LOCK_COMMIT["${name}|${larch}"]="${commit}"
    case "${LOCKED_NAMES}" in *" ${name} "*) ;; *) LOCKED_NAMES="${LOCKED_NAMES}${name} " ;; esac
done </tmpl/lock.tsv
# The lock row for a package in one pool, if any: its own architecture's, else all's.
lock_key() {
    if [ -n "${LOCK_VERSION[$1|$2]:-}" ]; then echo "$1|$2"
    elif [ -n "${LOCK_VERSION[$1|all]:-}" ]; then echo "$1|all"
    fi
}
# Where a package in one pool comes from: "<repository> <commit>" for an
# imported one, empty for one built here. Two archives with the same origin
# were released together from one commit -- the pair a package repository
# builds from one workspace -- and may pin each other exactly, the way local
# packages do; the lock boundary runs between DIFFERENT origins.
lock_origin() {
    local k
    k="$(lock_key "$1" "$2")"
    [ -z "${k}" ] || echo "${LOCK_REPO[${k}]} ${LOCK_COMMIT[${k}]}"
}

mapfile -t ROWS < /tmpl/producers.tsv
[ "${#ROWS[@]}" -gt 0 ] || [ "${LOCKED_NAMES}" != " " ] || {
    echo "error: /tmpl/producers.tsv is empty and the lock imports nothing, so every expectation below would be derived from nothing" >&2
    exit 1
}

LOCAL_NAMES=()
declare -A PKG_PRODUCER=()
declare -A PKG_DIR=()
declare -A WANTS_EXPECTED=()
declare -A PRODUCER_PACKAGES=()
declare -A PRODUCER_ARCHES=()
for row in "${ROWS[@]}"; do
    read -r producer dir arches packages enablement <<<"${row}"
    packages="$(printf '%s' "${packages}" | tr ',' ' ')"
    arches="$(printf '%s' "${arches}" | tr ',' ' ')"
    PRODUCER_PACKAGES["${producer}"]=" ${packages} "
    PRODUCER_ARCHES["${producer}"]=" ${arches} "

    # PACKAGES against the control templates present, both directions.
    mapfile -t templates < <(find "/tmpl/p/${producer}/control" -mindepth 1 -maxdepth 1 -type f -name '*.control' 2>/dev/null | LC_ALL=C sort)
    tmpl_names=""
    for t in ${templates[@]+"${templates[@]}"}; do
        n="$(awk '/^Package:/ { sub(/^Package:[[:space:]]*/, ""); print; exit }' "${t}")"
        [ -n "${n}" ] || {
            echo "error: ${dir}/control/$(basename "${t}") declares no Package:, so the package it describes has no name to check the pool against" >&2
            exit 1
        }
        tmpl_names="${tmpl_names}${n} "
    done
    want_sorted="$(printf '%s\n' ${packages} | LC_ALL=C sort | tr '\n' ' ')"
    got_sorted="$(printf '%s\n' ${tmpl_names} | LC_ALL=C sort | tr '\n' ' ')"
    [ "${want_sorted}" = "${got_sorted}" ] || {
        echo "error: the producer '${producer}' declares PACKAGES='${want_sorted% }' and ${dir}/control/ holds templates for '${got_sorted% }'. Those are two statements of one fact and they have come apart: pack.sh needs a template per package, and a template nothing declares is packed by nothing and expected by nothing" >&2
        exit 1
    }

    # ENABLEMENT needs a row per package; zero is spelled out.
    [ "${enablement}" != "-" ] || {
        echo "error: the producer '${producer}' declares no ENABLEMENT. Add ENABLEMENT='<package>=<count> ...' to ${dir}/producer.env with a row per package it emits, stating how many /etc/systemd/system/multi-user.target.wants symlinks that package ships -- 0 for a package that ships none. There is no default: a missing row and a deliberate zero look identical, and only one of them is a decision. See scripts/deb/README.md" >&2
        exit 1
    }
    declared=""
    for e in $(printf '%s' "${enablement}" | tr ',' ' '); do
        pkg="${e%%=*}"
        count="${e#*=}"
        [ -n "${pkg}" ] && [ "${pkg}" != "${e}" ] || {
            echo "error: the producer '${producer}' declares ENABLEMENT entry '${e}', which is not <package>=<count>" >&2
            exit 1
        }
        case "${count}" in
        '' | *[!0-9]*)
            echo "error: the producer '${producer}' declares ENABLEMENT '${e}', whose count is not a non-negative integer. It is the NUMBER of multi-user.target.wants symlinks that package ships" >&2
            exit 1
            ;;
        esac
        case " ${packages} " in
        *" ${pkg} "*) ;;
        *)
            echo "error: the producer '${producer}' declares ENABLEMENT for '${pkg}', which it does not emit. It emits: ${packages}" >&2
            exit 1
            ;;
        esac
        WANTS_EXPECTED["${pkg}"]="${count}"
        declared="${declared}${pkg} "
    done
    for pkg in ${packages}; do
        case " ${declared}" in
        *" ${pkg} "*) ;;
        *)
            echo "error: the producer '${producer}' emits '${pkg}' and its ENABLEMENT does not mention it. Add '${pkg}=<count>' to ${dir}/producer.env; a package left out has no stated enablement, and inferring one from what it happens to ship is what this gate exists to not do" >&2
            exit 1
            ;;
        esac
    done

    for pkg in ${packages}; do
        [ -z "${PKG_PRODUCER[${pkg}]:-}" ] || {
            echo "error: the package '${pkg}' is declared by two producers, '${PKG_PRODUCER[${pkg}]}' and '${producer}'. They write into one shared pool under one filename, so whichever builds second silently replaces the other" >&2
            exit 1
        }
        PKG_PRODUCER["${pkg}"]="${producer}"
        PKG_DIR["${pkg}"]="${dir}"
        LOCAL_NAMES+=("${pkg}")
    done
done

ARCHIVES_N=0
PATHS_N=0
SCRIPTS_N=0
# Reported on the RESULT line so a vacuous pass of (i) is visible; incremented
# only where the comparison or resolution actually happens.
ALL_COMPARED_N=0
VIRTUAL_RESOLVED_N=0
BUILT_VERSIONS=()
EXTERNALS=()
VIRTUALS=()
# sha256 of every Architecture: all archive, per pool, keyed <package>|<pool>.
declare -A ALL_SHA=()
ALL_PKGS=""

for arch in "${ARCHES[@]}"; do
    pool="/dist/${arch}/pool"
    [ -d "${pool}" ] || {
        echo "error: ${pool} does not exist, so there is nothing to check for ${arch}" >&2
        exit 1
    }
    mapfile -t debs < <(find "${pool}" -maxdepth 1 -type f -name '*.deb' -printf '%f\n' | LC_ALL=C sort)
    [ "${#debs[@]}" -gt 0 ] || {
        echo "error: ${pool} holds no .deb. Every assertion below is a property of the archives in it, and over an empty pool they are all true" >&2
        exit 1
    }
    ARCHIVES_N=$((ARCHIVES_N + ${#debs[@]}))

    # Producers building for this pool, including every `all` producer.
    pool_producers=""
    expected=""
    for producer in "${!PRODUCER_ARCHES[@]}"; do
        case "${PRODUCER_ARCHES[${producer}]}" in
        *" ${arch} "* | *" all "*)
            pool_producers="${pool_producers}${producer} "
            for pkg in ${PRODUCER_PACKAGES[${producer}]}; do
                # A locked package is expected from the lock, not the producer.
                [ -n "$(lock_key "${pkg}" "${arch}")" ] || expected="${expected}${pkg} "
            done
            ;;
        esac
    done
    pool_locked=""
    for key in "${!LOCK_VERSION[@]}"; do
        case "${key}" in
        *"|${arch}" | *"|all") pool_locked="${pool_locked}${key%%|*} " ;;
        esac
    done
    expected="${expected}${pool_locked}"
    EXPECTED_SET="$(printf '%s\n' ${expected} | LC_ALL=C sort -u | tr '\n' ' ')"

    got_names=()
    pool_versions=()
    # Package -> version in this pool, for the exact-pin check.
    unset POOL_PKG_VER
    declare -A POOL_PKG_VER=()
    for d in "${debs[@]}"; do
        n="$(dpkg-deb --field "${pool}/${d}" Package)"
        v="$(dpkg-deb --field "${pool}/${d}" Version)"
        got_names+=("${n}")
        pool_versions+=("${v}")
        POOL_PKG_VER["${n}"]="${v}"
    done

    got_set="$(printf '%s\n' "${got_names[@]}" | LC_ALL=C sort | tr '\n' ' ')"
    if [ "${got_set}" = "${EXPECTED_SET}" ]; then
        pass "${arch}: the pool holds exactly the packages its producers declare and the lock imports (${got_set% })"
    else
        fail "${arch}: the pool holds [${got_set% }], but the producers building for ${arch} declare and the lock imports [${EXPECTED_SET% }]. A missing package is one the composer cannot install; an extra one is an archive no producer owns and no lock row names. \`make os-pool\` fetches the imports and builds the rest"
    fi

    # h -- per producer, so a failure names the producer to look at.
    for producer in ${pool_producers}; do
        want_pkgs=""
        for pkg in ${PRODUCER_PACKAGES[${producer}]}; do
            [ -n "$(lock_key "${pkg}" "${arch}")" ] || want_pkgs="${want_pkgs} ${pkg}"
        done
        if [ -z "${want_pkgs}" ]; then
            pass "${arch}: the producer '${producer}' emits only packages the lock imports (${PRODUCER_PACKAGES[${producer}]# }), so nothing is expected from it here"
            continue
        fi
        found_n=0
        missing=""
        for pkg in ${want_pkgs}; do
            in_pool=0
            for g in "${got_names[@]}"; do
                [ "${pkg}" != "${g}" ] || in_pool=1
            done
            if [ "${in_pool}" = 1 ]; then
                found_n=$((found_n + 1))
            else
                missing="${missing} ${pkg}"
            fi
        done
        if [ "${found_n}" = 0 ]; then
            fail "${arch}: the producer '${producer}' contributed NO archive to ${pool}, though it declares [${want_pkgs# }] and builds for ${arch}. \`make os-debs\` runs every discovered producer; this one built nothing, or its output went somewhere else"
        elif [ -n "${missing}" ]; then
            fail "${arch}: the producer '${producer}' contributed only part of what it declares -- missing:${missing}. A producer emits its whole package set or the pool is a half-built one"
        else
            pass "${arch}: the producer '${producer}' contributed all of [${want_pkgs# }]"
        fi
    done
    orphan=""
    for g in "${got_names[@]}"; do
        [ -n "${PKG_PRODUCER[${g}]:-}" ] || [ -n "$(lock_key "${g}" "${arch}")" ] || orphan="${orphan} ${g}"
    done
    if [ -z "${orphan}" ]; then
        pass "${arch}: every archive in the pool maps back to a discovered producer or a lock row"
    else
        fail "${arch}: ${pool} holds archive(s) no discovered producer declares and no lock row names:${orphan}. Most likely a producer was deleted or renamed and its output was left behind; repo.sh indexes it and the composer would install it"
    fi

    # b -- imported archives must equal their lock rows; the rest must share
    # one well-formed stamp (everything after the last `+`).
    built_versions=()
    for d in "${debs[@]}"; do
        n="$(dpkg-deb --field "${pool}/${d}" Package)"
        v="$(dpkg-deb --field "${pool}/${d}" Version)"
        key="$(lock_key "${n}" "${arch}")"
        if [ -z "${key}" ]; then
            built_versions+=("${v}")
            continue
        fi
        actual_sha="$(sha256sum "${pool}/${d}" | cut -d' ' -f1)"
        actual_repo="$(dpkg-deb --field "${pool}/${d}" Mica-Source-Repo)"
        actual_commit="$(dpkg-deb --field "${pool}/${d}" Mica-Source-Commit)"
        if [ "${v}" = "${LOCK_VERSION[${key}]}" ] && [ "${actual_sha}" = "${LOCK_SHA[${key}]}" ] &&
            [ "${actual_repo}" = "${LOCK_REPO[${key}]}" ] && [ "${actual_commit}" = "${LOCK_COMMIT[${key}]}" ]; then
            pass "${n} ${arch}: imported, and is its lock row (${v} from ${actual_repo}@${actual_commit:0:12}, sha256 ${actual_sha:0:16})"
        else
            fail "${n} ${arch}: imported by deps/packages/${n}.json as ${LOCK_VERSION[${key}]} from ${LOCK_REPO[${key}]}@${LOCK_COMMIT[${key}]:0:12} (sha256 ${LOCK_SHA[${key}]:0:16}), but the pool holds ${v} from ${actual_repo:-?}@${actual_commit:0:12} (sha256 ${actual_sha:0:16}). A locked archive is its row or it is not in the pool; \`make os-pool\` refetches it"
        fi
    done
    pool_stamps=()
    for v in ${built_versions[@]+"${built_versions[@]}"}; do
        stamp="${v##*+}"
        case "${stamp}" in
        git[0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f]-* | \
            git[0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f].dirty-*)
            pool_stamps+=("${stamp}")
            ;;
        *)
            fail "${arch}: the version '${v}' carries no git<commit>[.dirty]-<rev> stamp after its last '+'. Every archive is stamped by scripts/deb/version.sh; a version without the stamp cannot be attributed to a commit"
            ;;
        esac
    done
    pool_stamp="$(printf '%s\n' ${pool_stamps[@]+"${pool_stamps[@]}"} | LC_ALL=C sort -u | tr '\n' ' ')"
    if [ "${#built_versions[@]}" -eq 0 ]; then
        pass "${arch}: every archive in the pool is imported by the lock; no built-here stamp to compare"
    elif [ "${#pool_stamps[@]}" -eq "${#built_versions[@]}" ] &&
        [ "$(printf '%s\n' "${pool_stamps[@]}" | LC_ALL=C sort -u | wc -l)" -eq 1 ]; then
        pass "${arch}: one git stamp across the archives built here (${pool_stamp% }, ${#built_versions[@]} archive(s))"
    else
        fail "${arch}: the archives built here carry more than one git stamp [${pool_stamp% }]. Rebuild them whole with \`make os-pool\`"
    fi
    BUILT_VERSIONS+=(${built_versions[@]+"${built_versions[@]}"})

    # Local-virtual names: what this pool's archives declare in Provides.
    declare -A PROVIDED_BY=()
    for d in "${debs[@]}"; do
        prov="$(dpkg-deb --field "${pool}/${d}" Provides)"
        pname="$(dpkg-deb --field "${pool}/${d}" Package)"
        IFS=',' read -ra pentries <<<"${prov}"
        for pe in ${pentries[@]+"${pentries[@]}"}; do
            read -r vname _rest <<<"${pe}"
            vname="${vname%%(*}"
            [ -n "${vname}" ] || continue
            PROVIDED_BY["${vname}"]="${PROVIDED_BY[${vname}]:-}${pname} "
        done
    done

    # a -- unversioned Conflicts only; a versioned one leaves a pair co-installable.
    declare -A CONFLICTS_WITH=()
    for d in "${debs[@]}"; do
        conf="$(dpkg-deb --field "${pool}/${d}" Conflicts)"
        pname="$(dpkg-deb --field "${pool}/${d}" Package)"
        IFS=',' read -ra centries <<<"${conf}"
        for ce in ${centries[@]+"${centries[@]}"}; do
            read -r cname crest <<<"${ce}"
            [ -z "${crest}" ] || continue
            case "${cname}" in *'('*) continue ;; esac
            [ -n "${cname}" ] || continue
            CONFLICTS_WITH["${pname}"]="${CONFLICTS_WITH[${pname}]:-}${cname} "
        done
    done

    # Every owner of a path, space-separated; a later claimant must conflict
    # with each of them.
    unset owner
    declare -A owner=()
    for d in "${debs[@]}"; do
        deb="${pool}/${d}"
        name="$(dpkg-deb --field "${deb}" Package)"
        version="$(dpkg-deb --field "${deb}" Version)"
        declared_arch="$(dpkg-deb --field "${deb}" Architecture)"
        depends="$(dpkg-deb --field "${deb}" Depends)"
        replaces="$(dpkg-deb --field "${deb}" Replaces)"

        # b/i -- architecture; `all` is correct in every pool.
        if [ "${declared_arch}" = "${arch}" ]; then
            pass "${name} ${arch}: declares Architecture: ${declared_arch}"
        elif [ "${declared_arch}" = all ]; then
            pass "${name} ${arch}: declares Architecture: all, a member of every pool"
            ALL_SHA["${name}|${arch}"]="$(sha256sum "${deb}" | cut -d' ' -f1)"
            case " ${ALL_PKGS} " in
            *" ${name} "*) ;;
            *) ALL_PKGS="${ALL_PKGS}${name} " ;;
            esac
        else
            fail "${name} in the ${arch} pool declares Architecture: ${declared_arch}. It was packed in the wrong container or filed under the wrong architecture"
        fi

        # a -- Replaces would let an overlap install cleanly, so it is refused.
        if [ -z "${replaces}" ]; then
            pass "${name} ${arch}: declares no Replaces"
        else
            fail "${name} ${arch}: declares Replaces: ${replaces}. That field exists to let one package take a path from another, which is the overlap the ownership check below refuses; no control template in this tree has one"
        fi

        # b/i -- the Depends closure, three classes.
        local_deps=" "
        IFS=',' read -ra entries <<<"${depends}"
        for entry in ${entries[@]+"${entries[@]}"}; do
            IFS='|' read -ra alts <<<"${entry}"
            for alt in "${alts[@]}"; do
                read -r dep_name _rest <<<"${alt}"
                dep_name="${dep_name%%(*}"
                [ -n "${dep_name}" ] || continue
                is_local=0
                for n in "${LOCAL_NAMES[@]}"; do
                    [ "${dep_name}" != "${n}" ] || is_local=1
                done
                case "${LOCKED_NAMES}" in *" ${dep_name} "*) is_local=1 ;; esac
                if [ "${is_local}" = 0 ]; then
                    # Local-virtual before external.
                    if [ -n "${PROVIDED_BY[${dep_name}]:-}" ]; then
                        VIRTUALS+=("${dep_name}")
                        VIRTUAL_RESOLVED_N=$((VIRTUAL_RESOLVED_N + 1))
                        pass "${name} ${arch}: depends on the local virtual ${dep_name}, provided in this pool by ${PROVIDED_BY[${dep_name}]% }"
                    else
                        EXTERNALS+=("${dep_name}")
                    fi
                    continue
                fi
                local_deps="${local_deps}${dep_name} "
                # The version of the package it names, not this archive's own.
                dep_ver="${POOL_PKG_VER[${dep_name}]:-}"
                across_lock=0
                [ "$(lock_origin "${name}" "${arch}")" = "$(lock_origin "${dep_name}" "${arch}")" ] || across_lock=1
                case "${alt}" in
                *"(= ${dep_ver:-<not in pool>})"*)
                    if [ "${across_lock}" = 1 ]; then
                        fail "${name} ${arch}: depends on ${dep_name} at the exact version (= ${dep_ver}) across the lock boundary. The two are released from different repositories (or different commits of one), so an exact pin holds only until either one moves; the dependency must be unversioned and the lock is what pins the pair"
                    else
                        pass "${name} ${arch}: depends on ${dep_name} at its exact pool version (= ${dep_ver})"
                    fi
                    ;;
                *'('*)
                    fail "${name} ${arch}: depends on the local package ${dep_name} as '${alt# }', which is neither unversioned nor that package's exact pool version (= ${dep_ver:-<not in pool>})"
                    ;;
                *)
                    if [ "${across_lock}" = 1 ]; then
                        pass "${name} ${arch}: depends on ${dep_name} unversioned across the lock boundary; the lock pins the pair"
                    else
                        fail "${name} ${arch}: depends on the local package ${dep_name} as '${alt# }', which is not that package's exact pool version (= ${dep_ver:-<not in pool>}). These are built from one commit across interfaces that carry no compatibility promise"
                    fi
                    ;;
                esac
                in_pool=0
                for g in "${got_names[@]}"; do
                    [ "${dep_name}" != "${g}" ] || in_pool=1
                done
                [ "${in_pool}" = 1 ] ||
                    fail "${name} ${arch}: depends on the local package ${dep_name}, which is not in ${pool}. The closure over our own packages has to be satisfiable from the pool itself"
            done
        done
        # Every other package of the micad workspace must depend on micad.
        case "${PKG_DIR[${name}]:-}" in
        pkgs/micad/*)
            if [ "${name}" != micad ]; then
                case "${local_deps}" in
                *" micad "*) pass "${name} ${arch}: pins micad" ;;
                *) fail "${name} ${arch}: declares no dependency on micad. Every package of the micad workspace but micad itself is built from micad's commit and speaks its interface" ;;
                esac
            fi
            ;;
        esac

        # The payload, read once and used by a, d and e.
        listing="$(dpkg-deb --contents "${deb}")"

        # a -- unique ownership of non-directory paths within one pool. The only
        # exemption is a set of packages that all declare mutual unversioned
        # Conflicts (no two can be co-installed); every exempted share is printed.
        dup=""
        exempt=""
        while read -r mode _own _size _date _time path _rest; do
            [ -n "${mode}" ] || continue
            case "${mode}" in
            d*) continue ;;
            esac
            path="${path#./}"
            path="${path%/}"
            [ -n "${path}" ] || continue
            PATHS_N=$((PATHS_N + 1))
            if [ -n "${owner[${path}]:-}" ]; then
                others="${owner[${path}]}"
                clique=1
                for other in ${others}; do
                    mutually_conflicting "${name}" "${other}" || clique=0
                done
                if [ "${clique}" = 1 ]; then
                    owner["${path}"]="${others} ${name}"
                    exempt="${exempt} /${path} (with ${others// /, })"
                else
                    dup="${dup} /${path} (also in ${others// /, })"
                fi
            else
                owner["${path}"]="${name}"
            fi
        done <<<"${listing}"
        [ -z "${exempt}" ] ||
            pass "${name} ${arch}: EXEMPT shared path(s), each claimed only by packages that declare mutual unversioned Conflicts and can never be co-installed:${exempt}"
        if [ -z "${dup}" ]; then
            pass "${name} ${arch}: owns no non-directory path another package in the pool owns${exempt:+, beyond the exempted share(s) above}"
        else
            fail "${name} ${arch}: ships path(s) another package already owns:${dup}. Two packages owning one file means whichever unpacks second wins, and there is no Replaces here to make that defined"
        fi

        # d -- copyright, present and non-empty.
        copy_size="$(awk -v p="./usr/share/doc/${name}/copyright" '$6 == p { print $3; exit }' <<<"${listing}")"
        if [ -n "${copy_size}" ] && [ "${copy_size}" -gt 0 ]; then
            pass "${name} ${arch}: ships /usr/share/doc/${name}/copyright (${copy_size} bytes)"
        else
            fail "${name} ${arch}: ships no non-empty /usr/share/doc/${name}/copyright (size: ${copy_size:-absent})"
        fi

        # e -- enablement links, counted as symlinks only.
        links="$(awk '$1 ~ /^l/ && $6 ~ /^\.\/etc\/systemd\/system\/multi-user\.target\.wants\// { print $6 }' <<<"${listing}" | LC_ALL=C sort | tr '\n' ' ')"
        link_n="$(awk '$1 ~ /^l/ && $6 ~ /^\.\/etc\/systemd\/system\/multi-user\.target\.wants\// { n++ } END { print n + 0 }' <<<"${listing}")"
        want="${WANTS_EXPECTED[${name}]:-}"
        if [ -z "${want}" ] && [ -n "$(lock_key "${name}" "${arch}")" ]; then
            pass "${name} ${arch}: imported; ships ${link_n} multi-user.target.wants symlink(s)${links:+ (${links% })}, asserted by its source repository's gate"
        elif [ -z "${want}" ]; then
            fail "${name} ${arch}: no producer declares an ENABLEMENT row for it, so this gate has no enablement expectation for it"
        elif [ "${link_n}" = "${want}" ]; then
            pass "${name} ${arch}: ${link_n} multi-user.target.wants symlink(s), as ${PKG_DIR[${name}]}/producer.env declares${links:+ (${links% })}"
        else
            fail "${name} ${arch}: ships ${link_n} multi-user.target.wants symlink(s)${links:+ (${links% })}, but ${PKG_DIR[${name}]}/producer.env declares ${want}. Either the payload gained a link nothing asked for, or lost one it needs, or the declaration is behind the package"
        fi

        # f and g -- the control archive.
        ctl="/work/${arch}/${name}"
        mkdir -p "${ctl}"
        dpkg-deb --control "${deb}" "${ctl}"
        if [ -e "${ctl}/conffiles" ]; then
            fail "${name} ${arch}: carries DEBIAN/conffiles. This root is an immutable dm-verity squashfs; a conffile promises dpkg a three-way merge against local edits that cannot exist and cannot be applied"
        else
            pass "${name} ${arch}: carries no DEBIAN/conffiles"
        fi
        for s in preinst postinst prerm postrm; do
            [ -f "${ctl}/${s}" ] || continue
            SCRIPTS_N=$((SCRIPTS_N + 1))
            if err="$(sh -n "${ctl}/${s}" 2>&1)"; then
                pass "${name} ${arch}: DEBIAN/${s} parses as POSIX sh"
            else
                fail "${name} ${arch}: DEBIAN/${s} is not valid POSIX sh: ${err}"
            fi
        done
    done
    unset PROVIDED_BY
    unset CONFLICTS_WITH
done

# i -- each `all` package is the same bytes in every pool.
for pkg in ${ALL_PKGS}; do
    seen=""
    where=""
    for arch in "${ARCHES[@]}"; do
        s="${ALL_SHA[${pkg}|${arch}]:-}"
        [ -n "${s}" ] || continue
        ALL_COMPARED_N=$((ALL_COMPARED_N + 1))
        where="${where}${arch}=${s:0:16} "
        case " ${seen} " in
        *" ${s} "*) ;;
        *) seen="${seen}${s} " ;;
        esac
    done
    if [ "$(printf '%s\n' ${seen} | wc -l)" -eq 1 ]; then
        pass "${pkg}: Architecture: all, byte-identical in every pool (${where% })"
    else
        fail "${pkg}: Architecture: all, but the pools hold DIFFERENT bytes under that one filename (${where% }). One build exports into every pool; two differing copies mean the composer installs a different package depending on which pool it resolved"
    fi
done

# One stamp across every pool, not only within each.
if [ "${#BUILT_VERSIONS[@]}" -eq 0 ]; then
    pass "every archive in every pool is imported by the lock; no built-here stamp to compare across pools"
else
all_stamps="$(printf '%s\n' "${BUILT_VERSIONS[@]}" | sed 's/^.*+//' | LC_ALL=C sort -u | tr '\n' ' ')"
if [ "$(printf '%s\n' "${BUILT_VERSIONS[@]}" | sed 's/^.*+//' | LC_ALL=C sort -u | wc -l)" -eq 1 ]; then
    pass "one git stamp across the archives built here in every pool (${all_stamps% })"
else
    fail "the archives built here carry more than one git stamp across the pools [${all_stamps% }]: they were not built from one commit"
fi
fi

echo "note: local virtual dependencies satisfied by a Provides in the pool: $(printf '%s\n' ${VIRTUALS[@]+"${VIRTUALS[@]}"} | LC_ALL=C sort -u | tr '\n' ' ')"
echo "note: external dependencies resolved by the composer, not by this pool: $(printf '%s\n' ${EXTERNALS[@]+"${EXTERNALS[@]}"} | LC_ALL=C sort -u | tr '\n' ' ')"
echo "GATE-COUNTS ${PASS_N} ${FAIL_N} ${ARCHIVES_N} ${PATHS_N} ${SCRIPTS_N} ${ALL_COMPARED_N} ${VIRTUAL_RESOLVED_N}"
INNER

read -r STATIC_PASS STATIC_FAIL ARCHIVES_N PATHS_N SCRIPTS_N ALL_COMPARED_N VIRTUAL_RESOLVED_N < <(sed -n 's/^GATE-COUNTS //p' "${STATIC_LOG}") || true
[ -n "${STATIC_PASS:-}" ] || {
    echo "error: the container run produced no GATE-COUNTS line, so nothing above it was actually asserted; its output is in ${STATIC_LOG}" >&2
    exit 1
}

if [ "${static_status}" != 0 ] || [ "${STATIC_FAIL}" != 0 ]; then
    # The rebuild would overwrite the failing archives, so stop here.
    echo "note: the reproducibility check was not run; fix the failures above first"
    echo "RESULT: FAIL ($((STATIC_PASS))/$((STATIC_PASS + STATIC_FAIL)) checks passed, ${ARCHIVES_N} archives, ${PATHS_N} payload paths, ${SCRIPTS_N} maintainer scripts, ${ALL_COMPARED_N} all-architecture archives compared, ${VIRTUAL_RESOLVED_N} local-virtual dependencies resolved)"
    exit 1
fi

# c -- two builds under one SOURCE_DATE_EPOCH are byte-identical. The rebuild
# runs on a freshly created builder (empty cache), and its log must contain
# pack.sh's own line for every archive compared, proving the layer was not
# replayed. Each architecture rebuilds a different producer.
REPRO_PASS=0
REPRO_FAIL=0
COMPARED_N=0
SKIPPED_ARCHES=0
GATE_BUILDERS=()
cleanup_builders() {
    for b in ${GATE_BUILDERS[@]+"${GATE_BUILDERS[@]}"}; do
        docker buildx rm "${b}" >/dev/null 2>&1 || true
    done
}
trap cleanup_builders EXIT

for i in "${!ARCHES[@]}"; do
    arch="${ARCHES[${i}]}"

    # The rows that build for this pool, in discovery order.
    candidates=()
    locked_names=" $(bash "${LOCK_SH}" --rows --arch "${arch}" | cut -f1 | tr '\n' ' ')"
    for row in "${ROWS[@]}"; do
        read -r producer dir arches packages _enablement <<<"${row}"
        case ",${arches}," in
        *",${arch},"* | *",all,"*) ;;
        *) continue ;;
        esac
        # Skip producers whose every package is imported.
        builds_here=0
        for p in $(printf '%s' "${packages}" | tr ',' ' '); do
            case "${locked_names}" in *" ${p} "*) ;; *) builds_here=1 ;; esac
        done
        [ "${builds_here}" = 1 ] || continue
        candidates+=("${row}")
    done
    # A pool that is imported entirely has nothing of this tree to rebuild; its
    # archives were held to their lock rows above. Only a tree with no pin at
    # all is refused here: then the pool came from nowhere this gate can see.
    [ "${#candidates[@]}" -gt 0 ] || {
        if [ -n "${locked_names// /}" ]; then
            echo "note: ${arch}: no discovered producer builds for this pool; every archive is imported at its lock row, so there is nothing to rebuild"
            SKIPPED_ARCHES=$((SKIPPED_ARCHES + 1))
            continue
        fi
        echo "error: no discovered producer builds for ${arch} and the lock names no ${arch} archive, yet ${DIST}/${arch}/pool was checked above. The reproducibility check would rebuild nothing" >&2
        exit 1
    }
    read -r producer dir arches packages _enablement <<<"${candidates[$((i % ${#candidates[@]}))]}"

    case ",${arches}," in
    *",all,"*) rebuild_arch=all ;;
    *) rebuild_arch="${arch}" ;;
    esac

    pool="${DIST}/${arch}/pool"
    before="${WORK}/before/${arch}"
    mkdir -p "${before}"
    names=()
    for p in $(printf '%s' "${packages}" | tr ',' ' '); do
        mapfile -t found < <(find "${pool}" -maxdepth 1 -type f \( -name "${p}_*_${arch}.deb" -o -name "${p}_*_all.deb" \) -printf '%f\n')
        [ "${#found[@]}" -eq 1 ] || {
            echo "error: ${pool} holds ${#found[@]} archives matching ${p}_*_{${arch},all}.deb; the reproducibility check needs exactly the one the rebuild will replace" >&2
            exit 1
        }
        cp "${pool}/${found[0]}" "${before}/${found[0]}"
        names+=("${found[0]}")
    done

    builder="mos-deb-gate-${arch}-$$"
    docker buildx create --name "${builder}" --driver docker-container >/dev/null
    GATE_BUILDERS+=("${builder}")

    log="${WORK}/rebuild-${arch}.log"
    echo "deb-package-gate: rebuilding the '${producer}' producer at --arch ${rebuild_arch} for the ${arch} pool on the empty-cache builder '${builder}'"
    rebuild_status=0
    BUILDX_BUILDER="${builder}" BUILDKIT_PROGRESS=plain \
        bash "${BUILD_SH}" --producer "${producer}" --arch "${rebuild_arch}" >"${log}" 2>&1 || rebuild_status=1
    if [ "${rebuild_status}" != 0 ]; then
        REPRO_FAIL=$((REPRO_FAIL + 1))
        echo "FAIL: ${arch}: the second build of the '${producer}' producer did not complete; its output is in ${log}"
        tail -n 20 "${log}"
        continue
    fi

    for n in "${names[@]}"; do
        COMPARED_N=$((COMPARED_N + 1))
        # pack.sh ran rather than a cached layer being replayed.
        if [ "$(grep -c "pack\.sh: ${n} " "${log}" || true)" -gt 0 ]; then
            REPRO_PASS=$((REPRO_PASS + 1))
            echo "PASS: ${arch}: the second build re-ran pack.sh for ${n} rather than replaying a cached layer"
        else
            REPRO_FAIL=$((REPRO_FAIL + 1))
            echo "FAIL: ${arch}: ${log} carries no 'pack.sh: ${n}' line, so the packing layer was served from cache and the comparison below is of an archive with itself. The empty-cache builder is what prevents this"
            continue
        fi
        if cmp -s "${before}/${n}" "${pool}/${n}"; then
            REPRO_PASS=$((REPRO_PASS + 1))
            echo "PASS: ${arch}: ${n} is byte-identical across two builds at one SOURCE_DATE_EPOCH"
        else
            REPRO_FAIL=$((REPRO_FAIL + 1))
            echo "FAIL: ${arch}: ${n} differs between two builds at one SOURCE_DATE_EPOCH ($(stat -c%s "${before}/${n}") bytes then, $(stat -c%s "${pool}/${n}") bytes now). Something in the packing path is not a function of its inputs"
        fi
    done

    docker buildx rm "${builder}" >/dev/null 2>&1 || true
done

[ "${COMPARED_N}" -gt 0 ] || [ "${SKIPPED_ARCHES}" -eq "${#ARCHES[@]}" ] || {
    echo "error: no archive was rebuilt and compared, so the reproducibility check asserted nothing" >&2
    exit 1
}
[ "${COMPARED_N}" -gt 0 ] ||
    echo "note: every pool is imported entirely; the reproducibility check rebuilt nothing because this tree builds nothing"

PASS_N=$((STATIC_PASS + REPRO_PASS))
FAIL_N=$((STATIC_FAIL + REPRO_FAIL))
echo "RESULT: $([ "${FAIL_N}" -eq 0 ] && echo PASS || echo FAIL) ($((PASS_N))/$((PASS_N + FAIL_N)) checks passed, ${ARCHIVES_N} archives, ${PATHS_N} payload paths, ${SCRIPTS_N} maintainer scripts, ${COMPARED_N} rebuilt archives compared, ${ALL_COMPARED_N} all-architecture archives compared, ${VIRTUAL_RESOLVED_N} local-virtual dependencies resolved)"
[ "${FAIL_N}" -eq 0 ]

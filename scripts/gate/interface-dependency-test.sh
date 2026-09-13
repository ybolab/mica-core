#!/usr/bin/env bash
# PROPOSAL acceptance case, not wired into `make check`: an API-only update
# installs mica-apid@B beside the exact micad@A archive already installed, and
# dpkg accepts the pair only when micad@A provides the D-Bus interface revision
# mica-apid@B requires (docs/plan/20260913-2230-independent-apid-upgrade.md).
#
# Synthetic archives, packed by this repository's scripts/deb/pack.sh inside
# the pinned IMAGE_MICA_BUILD_BASE and installed with that image's dpkg into a
# fresh --root/--admindir per case; no apt, no network inside the container.
# Each case asserts dpkg's exit status AND the package's recorded state, so a
# refusal that still left the package installed is a failure (fail-closed).
#
# dpkg checks a package's own Depends when it configures that package, and does
# NOT re-check installed reverse dependencies when another package is replaced:
# a micad of a lower revision unpacks and configures over micad@A under an
# installed mica-apid@B. So the proposal adds a closure check over the dpkg
# database after the last install (`closure` below, dpkg-checkbuilddeps
# --admindir over every installed package's Pre-Depends and Depends), and the
# replace case asserts that check refuses the root.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
command -v docker >/dev/null 2>&1 || { echo "error: docker is required and not on PATH" >&2; exit 1; }
IMAGE="$(bash "${REPO_ROOT}/scripts/build/from.sh" --arch=amd64 --ref IMAGE_MICA_BUILD_BASE)"

echo "=== interface dependency acceptance in ${IMAGE} ==="
# mica-build-side: container-block -- dpkg and pack.sh of the pinned base image.
docker run --rm --label ai-agent=true --platform linux/amd64 \
    -v "${REPO_ROOT}/scripts/deb:/packer:ro" \
    --entrypoint /bin/bash "${IMAGE}" -c '
set -euo pipefail
echo "dpkg $(sed -n "s/^MICA_BUILD_DPKG=//p" /etc/mica-build/base.env) from $(sed -n "s/^MICA_BUILD_IMAGE=//p" /etc/mica-build/base.env)"
export SOURCE_DATE_EPOCH=1757721600 MICA_DEB_SOURCE_REPO=mica-core
A=0.1.0+gitaaaaaaaaaaaa-1
B=0.1.0+gitbbbbbbbbbbbb-1
POOL=/work/pool
mkdir -p "${POOL}"

# pkg <name> <version> <commit12> [control line ...]
pkg() {
    local name="$1" version="$2" commit="$3"
    shift 3
    local root="/work/stage/${name}-${version}"
    install -D -m 0644 /dev/null "${root}/usr/share/doc/${name}/copyright"
    printf "synthetic acceptance archive\n" >"${root}/usr/share/doc/${name}/copyright"
    {
        printf "Package: %s\nVersion: @VERSION@\nArchitecture: @ARCH@\n" "${name}"
        printf "Maintainer: mica build <mica@example.invalid>\nSection: admin\nPriority: optional\n"
        printf "%s\n" "$@"
        printf "Description: synthetic %s\n acceptance archive\n" "${name}"
    } >"/work/${name}-${version}.control"
    MICA_DEB_SOURCE_COMMIT="${commit}${commit}${commit}${commit:0:4}" bash /packer/pack.sh --root "${root}" \
        --control "/work/${name}-${version}.control" --version "${version}" --arch amd64 --out "/work/out-${name}-${version}" >/dev/null
    mv /work/out-"${name}-${version}"/*.deb "${POOL}/"
}

# micad@A as shipped under the proposal: interface com.mica.micad1 at revision 3.
pkg micad "${A}" aaaaaaaaaaaa "Provides: com.mica.micad1 (= 3)"
# micad@A2: a later micad of revision 2 (older interface), for the replace case.
pkg micad 0.0.9+gitcccccccccccc-1 cccccccccccc "Provides: com.mica.micad1 (= 2)"
# micad@M: a breaking micad that serves only the next major interface.
pkg micad 0.2.0+gitdddddddddddd-1 dddddddddddd "Provides: com.mica.micad2 (= 1)"
# mica-apid@B needing revision 3, and one needing revision 4.
pkg mica-apid "${B}" bbbbbbbbbbbb "Depends: com.mica.micad1 (>= 3)"
pkg mica-apid 0.1.1+giteeeeeeeeeeee-1 eeeeeeeeeeee "Depends: com.mica.micad1 (>= 4)"
# Today'"'"'s coupling, for contrast: an exact pin on the micad of its own commit.
pkg mica-apid 0.1.0+gitffffffffffff-1 ffffffffffff "Depends: micad (= 0.1.0+gitffffffffffff-1)"
ls "${POOL}" | sed "s/^/pool: /"

deb() { ls "${POOL}/$1_$2_amd64.deb"; }
fresh() {
    rm -rf /work/root
    mkdir -p /work/root/var/lib/dpkg/info /work/root/var/lib/dpkg/updates
    : >/work/root/var/lib/dpkg/status
    : >/work/root/var/lib/dpkg/available
}
dpkgr() { dpkg --root=/work/root --admindir=/work/root/var/lib/dpkg --force-script-chrootless --log=/dev/null "$@"; }
state() { dpkg-query --admindir=/work/root/var/lib/dpkg -W -f="\${db:Status-Abbrev}\${Version}" "$1" 2>/dev/null || true; }
# closure: the Pre-Depends and Depends of every installed package, satisfied by the
# database itself. dpkg-checkbuilddeps evaluates a relation string against
# --admindir; the dummy control file only gives it a source to name.
printf "Source: closure\n\nPackage: closure\nArchitecture: all\n" >/work/closure.control
closure() {
    local bad_n=0 p deps
    while IFS= read -r p; do
        deps="$(dpkg-query --admindir=/work/root/var/lib/dpkg -W -f="\${Pre-Depends}, \${Depends}" "${p}" | sed "s/^[, ]*//; s/[, ]*$//")"
        [ -n "${deps}" ] || continue
        if ! dpkg-checkbuilddeps --admindir=/work/root/var/lib/dpkg -d "${deps}" /work/closure.control 2>/work/closure.log; then
            echo "closure: ${p}: $(grep -v "cannot determine CC" /work/closure.log | sed "s/^dpkg-checkbuilddeps: error: //")"
            bad_n=$((bad_n + 1))
        fi
    done < <(dpkg-query --admindir=/work/root/var/lib/dpkg -W -f="\${db:Status-Abbrev}\${Package}\n" | sed -n "s/^ii *//p")
    [ "${bad_n}" = 0 ]
}
PASS=0
FAIL=0
ok() { PASS=$((PASS + 1)); echo "PASS: $*"; }
bad() { FAIL=$((FAIL + 1)); echo "FAIL: $*"; }
# expect <accept|refuse> <package> <version> <description> -- dpkg args
expect() {
    local want="$1" p="$2" v="$3" what="$4" rc=0 got
    shift 5
    dpkgr "$@" >/work/dpkg.log 2>&1 || rc=$?
    got="$(state "${p}")"
    if [ "${want}" = accept ] && [ "${rc}" = 0 ] && [ "${got}" = "ii ${v}" ]; then
        ok "${what} (dpkg exit 0, ${p} ${got})"
    elif [ "${want}" = refuse ] && [ "${rc}" != 0 ] && [ "${got}" != "ii ${v}" ]; then
        ok "${what} (dpkg exit ${rc}, ${p} ${got:-not installed}): $(grep -m1 "depends on\|dependency" /work/dpkg.log | sed "s/^ *//")"
    else
        bad "${what}: wanted ${want}, dpkg exit ${rc}, ${p} state [${got}]"
        sed "s/^/    /" /work/dpkg.log
    fi
}

fresh
expect accept micad "${A}" "micad@A installs alone" -- -i "$(deb micad "${A}")"
expect accept mica-apid "${B}" "mica-apid@B (>= 3) is accepted beside the exact micad@A archive (= 3)" -- -i "$(deb mica-apid "${B}")"
[ "$(state micad)" = "ii ${A}" ] && ok "micad@A is still the installed micad, untouched by the API-only update" || bad "micad changed: $(state micad)"
closure && ok "the closure check accepts micad@A + mica-apid@B" || bad "the closure check refused micad@A + mica-apid@B"

fresh
dpkgr -i "$(deb micad "${A}")" >/dev/null
expect refuse mica-apid 0.1.1+giteeeeeeeeeeee-1 "mica-apid needing revision 4 is refused against micad@A (revision 3)" -- -i "$(deb mica-apid 0.1.1+giteeeeeeeeeeee-1)"

fresh
dpkgr -i "$(deb micad 0.2.0+gitdddddddddddd-1)" >/dev/null
expect refuse mica-apid "${B}" "mica-apid@B (com.mica.micad1) is refused against a micad serving only com.mica.micad2" -- -i "$(deb mica-apid "${B}")"

fresh
expect refuse mica-apid "${B}" "mica-apid@B is refused with no micad at all" -- -i "$(deb mica-apid "${B}")"

fresh
dpkgr -i "$(deb micad "${A}")" "$(deb mica-apid "${B}")" >/dev/null
expect accept micad 0.0.9+gitcccccccccccc-1 "dpkg alone ACCEPTS replacing micad@A by revision 2 under an installed mica-apid@B" -- -i "$(deb micad 0.0.9+gitcccccccccccc-1)"
if out="$(closure)"; then
    bad "the closure check accepted mica-apid@B over a micad of revision 2"
else
    ok "the closure check refuses that root: ${out}"
fi

fresh
dpkgr -i "$(deb micad "${A}")" >/dev/null
expect refuse mica-apid 0.1.0+gitffffffffffff-1 "today: an exact-pinned mica-apid is refused against any other commit'"'"'s micad" -- -i "$(deb mica-apid 0.1.0+gitffffffffffff-1)"

echo "RESULT: ${PASS} passed, ${FAIL} failed"
[ "${FAIL}" = 0 ]
'

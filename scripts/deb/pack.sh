#!/usr/bin/env bash
# Turn a staged filesystem tree into one binary Debian package.
#
#   MICA_DEB_SOURCE_REPO=<repository> MICA_DEB_SOURCE_COMMIT=<sha> SOURCE_DATE_EPOCH=<s> \
#   pack.sh --root <dir> --control <template> --version <v> \
#           --arch <amd64|arm64|all> --out <dir> [--maintainer-scripts <dir>]
#
# Runs inside the IMAGE_MICA_BUILD_BASE image at the package's architecture:
# dpkg-shlibdeps resolves against the libraries of the container it runs in.
# scripts/deb/README.md is the contract this implements.
#
# mica-build-side: container -- every line below runs in the IMAGE_MICA_BUILD_BASE image,
# started by a producer Dockerfile's RUN; the architecture check further down is what
# refuses a run that reached here any other way.
set -euo pipefail

die() {
    echo "pack.sh: error: $*" >&2
    exit 1
}

ROOT=""
CONTROL=""
VERSION=""
ARCH=""
OUT=""
SCRIPTS=""
while [ "$#" -gt 0 ]; do
    case "$1" in
    --root) ROOT="${2-}"; [ -n "${ROOT}" ] || die "--root takes the staged tree directory"; shift 2 ;;
    --control) CONTROL="${2-}"; [ -n "${CONTROL}" ] || die "--control takes a control template"; shift 2 ;;
    --version) VERSION="${2-}"; [ -n "${VERSION}" ] || die "--version takes a Debian version"; shift 2 ;;
    --arch) ARCH="${2-}"; [ -n "${ARCH}" ] || die "--arch takes amd64, arm64 or all"; shift 2 ;;
    --out) OUT="${2-}"; [ -n "${OUT}" ] || die "--out takes a directory"; shift 2 ;;
    --maintainer-scripts) SCRIPTS="${2-}"; [ -n "${SCRIPTS}" ] || die "--maintainer-scripts takes a directory"; shift 2 ;;
    *) die "'$1' is not an option this packer takes; see scripts/deb/README.md" ;;
    esac
done
for pair in "root:${ROOT}" "control:${CONTROL}" "version:${VERSION}" "arch:${ARCH}" "out:${OUT}"; do
    [ -n "${pair#*:}" ] || die "--${pair%%:*} is required"
done

# No "now" default: it would make every archive irreproducible.
[ -n "${SOURCE_DATE_EPOCH:-}" ] ||
    die "SOURCE_DATE_EPOCH is unset. This packer sets every payload mtime to it and hands it to dpkg-deb; there is no 'now' default, because a silent one makes every package irreproducible while looking green"
case "${SOURCE_DATE_EPOCH}" in
'' | *[!0-9]*) die "SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH} is not a whole number of seconds since the epoch" ;;
esac

# Provenance, resolved on the host by deb/build.sh and written as the
# Mica-Source-Repo and Mica-Source-Commit control fields. No defaults.
[ -n "${MICA_DEB_SOURCE_REPO:-}" ] ||
    die "MICA_DEB_SOURCE_REPO is unset. This packer records the source repository in the control file; scripts/deb/build.sh resolves it from the repository's origin and the producer Dockerfile must declare it as an ARG"
[ -n "${MICA_DEB_SOURCE_COMMIT:-}" ] ||
    die "MICA_DEB_SOURCE_COMMIT is unset. This packer records the source commit in the control file; scripts/deb/build.sh resolves it from HEAD and the producer Dockerfile must declare it as an ARG"
[[ "${MICA_DEB_SOURCE_REPO}" =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ ]] ||
    die "MICA_DEB_SOURCE_REPO='${MICA_DEB_SOURCE_REPO}' is not a repository name (letters, digits, dot, underscore, minus)"
[[ "${MICA_DEB_SOURCE_COMMIT}" =~ ^[0-9a-f]{40}$ ]] ||
    die "MICA_DEB_SOURCE_COMMIT='${MICA_DEB_SOURCE_COMMIT}' is not a full 40-hex commit id"

[ -d "${ROOT}" ] || die "--root ${ROOT} is not a directory"
[ -n "$(ls -A "${ROOT}")" ] || die "--root ${ROOT} is empty; a package with no payload installs nothing and reports success"
[ ! -e "${ROOT}/DEBIAN" ] || die "--root ${ROOT} already carries a DEBIAN directory. This packer owns DEBIAN -- control, md5sums and the maintainer scripts -- and a staged one would be silently merged with what it writes"
[ -f "${CONTROL}" ] || die "--control ${CONTROL} is not a file"
case "${ARCH}" in
amd64 | arm64 | all) ;;
*) die "--arch ${ARCH} is not amd64, arm64 or all" ;;
esac

CONTAINER_ARCH="$(dpkg --print-architecture)"
if [ "${ARCH}" != all ] && [ "${ARCH}" != "${CONTAINER_ARCH}" ]; then
    die "--arch ${ARCH} in a ${CONTAINER_ARCH} container. dpkg-shlibdeps resolves the libraries of the architecture it RUNS on, so this would record ${CONTAINER_ARCH}'s dependency versions in an ${ARCH} package and fail on the device, not here. Run this inside the ${ARCH} image of IMAGE_MICA_BUILD_BASE"
fi

mkdir -p "${OUT}"
OUT="$(cd "${OUT}" && pwd)"
ROOT="$(cd "${ROOT}" && pwd)"

# One field of a control stanza, continuation lines folded onto one line.
control_field() {
    awk -v want="$1" '
        /^[^ \t]/ {
            i = index($0, ":")
            if (i == 0) next
            f = substr($0, 1, i - 1)
            v = substr($0, i + 1)
            sub(/^[ \t]+/, "", v)
            if (f == want) { got = 1; out = v; inf = 1 } else { inf = 0 }
            next
        }
        inf { line = $0; sub(/^[ \t]+/, " ", line); out = out line }
        END { if (got) print out }
    ' "$2"
}

# The template, checked before anything is rendered from it.
for f in Package Version Architecture Maintainer Section Priority Description; do
    [ -n "$(control_field "${f}" "${CONTROL}")" ] ||
        die "--control ${CONTROL} declares no ${f}. Every field of the template is required; scripts/deb/README.md lists them and shows one"
done
if grep -ci '^Installed-Size:' "${CONTROL}" >/dev/null; then
    die "--control ${CONTROL} declares Installed-Size. That field is COMPUTED here from the staged tree; a written one is a number that stops matching the payload the first time the payload changes"
fi
case "$(control_field Version "${CONTROL}")" in
*'@VERSION@'*) ;;
*) die "--control ${CONTROL} has a Version: that does not contain @VERSION@, so --version ${VERSION} would be discarded and the template's own value shipped" ;;
esac
case "$(control_field Architecture "${CONTROL}")" in
*'@ARCH@'*) ;;
*) die "--control ${CONTROL} has an Architecture: that does not contain @ARCH@, so --arch ${ARCH} would be discarded and the template's own value shipped" ;;
esac
# The provenance fields are written by this packer, never by the template.
for f in Mica-Source-Repo Mica-Source-Commit; do
    [ -z "$(control_field "${f}" "${CONTROL}")" ] ||
        die "--control ${CONTROL} declares ${f}. That field is WRITTEN here from MICA_DEB_SOURCE_REPO and MICA_DEB_SOURCE_COMMIT; a written one is a value that stops matching the build the first time the archive is built from another checkout"
done

WORK="$(mktemp -d)"
trap 'rm -rf "${WORK}"' EXIT

sed -e "s|@VERSION@|${VERSION}|g" -e "s|@ARCH@|${ARCH}|g" "${CONTROL}" >"${WORK}/control"
PACKAGE="$(control_field Package "${WORK}/control")"
[[ "${PACKAGE}" =~ ^[a-z0-9][a-z0-9+.-]+$ ]] ||
    die "'${PACKAGE}' is not a Debian package name (lowercase letters, digits, plus, minus and dot, at least two characters)"

# dpkg-shlibdeps expects debian/<package>/ and a debian/control beside it;
# --ignore-missing-info would hide unresolved libraries.
PKG_DIR="${WORK}/debian/${PACKAGE}"
mkdir -p "${PKG_DIR}" "${PKG_DIR}/DEBIAN"
cp -a "${ROOT}/." "${PKG_DIR}/"
printf 'Source: %s\n\nPackage: %s\nArchitecture: %s\n' "${PACKAGE}" "${PACKAGE}" "${ARCH}" >"${WORK}/debian/control"

case "$(control_field Depends "${WORK}/control")" in
*'${shlibs:Depends}'*)
    # ELF payloads found with `file`, not from a caller-kept list.
    ELVES=()
    while IFS= read -r -d '' f; do
        case "$(file -b "${f}")" in
        ELF*) ELVES+=("${f}") ;;
        esac
    done < <(find "${PKG_DIR}" -path "${PKG_DIR}/DEBIAN" -prune -o -type f -print0)
    [ "${#ELVES[@]}" -gt 0 ] ||
        die "the control template asks for \${shlibs:Depends}, but no ELF executable or shared object was staged under --root ${ROOT}. The substitution would expand to nothing and leave a dangling separator in Depends"

    # DEB_HOST_ARCH/DEB_BUILD_ARCH are set because dpkg-architecture otherwise
    # asks the C compiler what it targets, and this image deliberately has none.
    SHLIBS_OUT="$(cd "${WORK}" && DEB_HOST_ARCH="${CONTAINER_ARCH}" DEB_BUILD_ARCH="${CONTAINER_ARCH}" \
        dpkg-shlibdeps -O "${ELVES[@]}")"
    SHLIBS="$(printf '%s\n' "${SHLIBS_OUT}" | sed -n 's/^shlibs:Depends=//p')"
    [ -n "${SHLIBS}" ] ||
        die "dpkg-shlibdeps found no shared-library dependency across ${#ELVES[@]} ELF payload(s), but the template asks for \${shlibs:Depends}. Either the payload is statically linked -- in which case the template should not ask -- or shlibdeps resolved nothing it should have"

    # A literal replacement, only inside the Depends field including its
    # continuation lines (the same extent control_field reads); elsewhere the
    # token is prose and is shipped as written.
    awk -v rep="${SHLIBS}" '
        BEGIN { tok = "${shlibs:Depends}" }
        /^[^ \t]/ {
            i = index($0, ":")
            in_depends = (i > 0 && substr($0, 1, i - 1) == "Depends")
        }
        in_depends {
            while ((i = index($0, tok)) > 0) $0 = substr($0, 1, i - 1) rep substr($0, i + length(tok))
        }
        { print }
    ' "${WORK}/control" >"${WORK}/control.subst"
    mv "${WORK}/control.subst" "${WORK}/control"
    ;;
esac

# Installed-Size, by the rule dpkg-gencontrol applies: a file or symlink costs
# ceil(bytes / 1024) KiB and everything else costs one, DEBIAN excluded because
# it is not installed.
INSTALLED_SIZE="$(cd "${PKG_DIR}" && find . -mindepth 1 -path ./DEBIAN -prune -o -printf '%y %s\n' | awk '
    $1 == "f" || $1 == "l" { total += int(($2 + 1023) / 1024); next }
    { total += 1 }
    END { print total + 0 }
')"
[ -n "${INSTALLED_SIZE}" ] || die "could not measure the staged tree under ${ROOT}"
# Through awk, not >>: a trailing blank line would start a stanza dpkg ignores.
awk -v size="${INSTALLED_SIZE}" -v repo="${MICA_DEB_SOURCE_REPO}" -v commit="${MICA_DEB_SOURCE_COMMIT}" '
    { line[NR] = $0; if (NF) last = NR }
    END {
        for (i = 1; i <= last; i++) print line[i]
        print "Installed-Size: " size
        print "Mica-Source-Repo: " repo
        print "Mica-Source-Commit: " commit
    }
' "${WORK}/control" >"${WORK}/control.sized"
install -m 0644 "${WORK}/control.sized" "${PKG_DIR}/DEBIAN/control"

# md5sums over the payload, paths relative and without a leading ./ -- the shape
# dpkg reads back with `dpkg --verify`.
(cd "${PKG_DIR}" && find . -path ./DEBIAN -prune -o -type f -printf '%P\0' |
    LC_ALL=C sort -z | xargs -0 -r md5sum) >"${PKG_DIR}/DEBIAN/md5sums"
chmod 0644 "${PKG_DIR}/DEBIAN/md5sums"

if [ -n "${SCRIPTS}" ]; then
    [ -d "${SCRIPTS}" ] || die "--maintainer-scripts ${SCRIPTS} is not a directory"
    found=0
    while IFS= read -r f; do
        n="$(basename "${f}")"
        case "${n}" in
        preinst | postinst | prerm | postrm) ;;
        *) die "--maintainer-scripts ${SCRIPTS} holds '${n}', which dpkg does not run. Only preinst, postinst, prerm and postrm are installed; a file with any other name would be carried into DEBIAN and silently never executed" ;;
        esac
        [ -f "${f}" ] || die "--maintainer-scripts ${SCRIPTS}/${n} is not a regular file"
        install -m 0755 "${f}" "${PKG_DIR}/DEBIAN/${n}"
        found=$((found + 1))
    done < <(find "${SCRIPTS}" -mindepth 1 -maxdepth 1 | LC_ALL=C sort)
    [ "${found}" -gt 0 ] ||
        die "--maintainer-scripts ${SCRIPTS} holds none of preinst, postinst, prerm, postrm; passing an empty directory reads like maintainer actions were installed"
fi

# Normalisation, last. Every mtime is SET to SOURCE_DATE_EPOCH, not clamped:
# copied files carry their checkout time, which a clamp would keep.
chown -Rh root:root "${PKG_DIR}"
find "${PKG_DIR}" -print0 |
    xargs -0 -r touch --no-dereference --date="@${SOURCE_DATE_EPOCH}"

DEB="${OUT}/${PACKAGE}_${VERSION}_${ARCH}.deb"
rm -f "${DEB}"
dpkg-deb --build --root-owner-group "${PKG_DIR}" "${DEB}" >/dev/null

# The archive, read back through dpkg-deb.
got_package="$(dpkg-deb --field "${DEB}" Package)"
got_version="$(dpkg-deb --field "${DEB}" Version)"
got_arch="$(dpkg-deb --field "${DEB}" Architecture)"
got_size="$(dpkg-deb --field "${DEB}" Installed-Size)"
got_depends="$(dpkg-deb --field "${DEB}" Depends)"
got_repo="$(dpkg-deb --field "${DEB}" Mica-Source-Repo)"
got_commit="$(dpkg-deb --field "${DEB}" Mica-Source-Commit)"
[ "${got_package}" = "${PACKAGE}" ] || die "${DEB} declares Package: ${got_package}, not ${PACKAGE}"
[ "${got_version}" = "${VERSION}" ] || die "${DEB} declares Version: ${got_version}, not the --version ${VERSION} it was built with"
[ "${got_arch}" = "${ARCH}" ] || die "${DEB} declares Architecture: ${got_arch}, not the --arch ${ARCH} it was built with"
[ "${got_size}" = "${INSTALLED_SIZE}" ] || die "${DEB} declares Installed-Size: ${got_size}, but the staged tree measures ${INSTALLED_SIZE}"
[ "${got_repo}" = "${MICA_DEB_SOURCE_REPO}" ] || die "${DEB} declares Mica-Source-Repo: ${got_repo}, not ${MICA_DEB_SOURCE_REPO}"
[ "${got_commit}" = "${MICA_DEB_SOURCE_COMMIT}" ] || die "${DEB} declares Mica-Source-Commit: ${got_commit}, not ${MICA_DEB_SOURCE_COMMIT}"
case "${got_depends}" in
*'${'*) die "${DEB} declares Depends: ${got_depends}, which still carries an unexpanded substitution variable. APT would refuse it, or worse, parse the literal as a package name" ;;
esac

not_root="$(dpkg-deb --contents "${DEB}" | awk '$2 != "root/root" { print $2 " " $6 }')"
[ -z "${not_root}" ] ||
    die "${DEB} carries paths that are not root/root, so the installed root would inherit a build user: ${not_root}"

# --quoting-style=literal so tar lists non-ASCII paths as the raw bytes find prints.
dpkg-deb --fsys-tarfile "${DEB}" | tar --quoting-style=literal -tf - | sed -e 's|^\./||' -e 's|/$||' -e '/^$/d' |
    LC_ALL=C sort >"${WORK}/payload.paths"
(cd "${ROOT}" && find . -mindepth 1 -printf '%P\n') | LC_ALL=C sort >"${WORK}/staged.paths"
if ! diff -u "${WORK}/staged.paths" "${WORK}/payload.paths" >"${WORK}/paths.diff"; then
    echo "pack.sh: error: the payload of ${DEB} is not the tree staged under ${ROOT} (-- staged, ++ packed):" >&2
    sed -n '3,23p' "${WORK}/paths.diff" >&2
    exit 1
fi

printf 'pack.sh: %s  %s bytes  Depends: %s\n' \
    "$(basename "${DEB}")" "$(stat -c%s "${DEB}")" "${got_depends:-(none)}"

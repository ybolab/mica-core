# syntax=docker/dockerfile:1@sha256:ecfaec9ed6d810b56388c508f4121597bfbba70d41a6dfeee4d8cad5f295fc32
# check=skip=InvalidDefaultArgInFrom
# The mosd producer: the `mosd` and `mos-apid` packages.
#
#   make os-deb-mosd   ->  build-env/deb/build.sh --producer mosd --arch <amd64|arm64>
#
# This file only STAGES and PACKS. The binaries were cross-compiled on an amd64
# Rust builder by this producer's PREPARE hook and arrive through the `bin`
# context; the shared copyright file arrives through `family`, because the build
# context is this producer's own directory. This
# stage runs at the TARGET architecture because dpkg-shlibdeps resolves an
# ELF's dependencies against the libraries of the container it runs in, and
# build-env/deb/pack.sh refuses the mismatch by name.
#
# No default for the base, for the reason build-env/deb/Dockerfile gives: a
# driver that failed to pass the argument would otherwise build from a floating
# tag.
ARG MOS_BUILD_DEB

FROM ${MOS_BUILD_DEB} AS pack

# pack.sh comes over as a named context rather than out of the image. It is the
# packaging CONTRACT, shared by every producer, and build-env/deb/README.md
# requires this shape: an edit to the packer then takes effect without
# rebuilding the whole builder family.
COPY --from=packer pack.sh /usr/local/bin/pack.sh

COPY --from=family copyright /in/copyright
COPY control/ /in/control/
COPY --from=dist mosd.service apid.service com.mos.mosd.conf /in/dist/
COPY --from=bin mosd apid /in/bin/

ARG MOS_DEB_VERSION
ARG MOS_DEB_ARCH
ARG SOURCE_DATE_EPOCH

# Two staged roots and two pack.sh runs. The paths, the modes and the
# enablement links are the ones rootfs/scripts/mosd-install.sh writes today,
# so a root composed from these packages is the tree the stage chain installs.
#
# ENABLEMENT IS OWNED BY THE PACKAGE. Each package ships its own
# multi-user.target.wants symlink as a FILE IN ITS PAYLOAD -- not as something
# a maintainer script creates. There is no postinst here and nothing calls
# `systemctl enable`: a unit that is enabled by a script is enabled by
# something a reader has to run to see, while a symlink in the archive is
# visible to `dpkg-deb --contents` and to every gate that reads one.
#
# NOT a conffile, deliberately. These two links live under /etc, which is where
# dpkg would normally expect configuration a user edits and preserves across
# upgrades -- but this root filesystem is an immutable dm-verity squashfs and
# nobody edits anything in it. A conffiles entry would promise a merge that
# cannot happen. Do not "fix" this into one.
#
# /usr/share/doc/<package>/copyright is per package and not shared, and each
# wants-symlink names its own unit, which is what keeps the two payloads
# disjoint: no path is owned by both.
RUN set -eu; \
    install -D -m 0755 /in/bin/mosd               /stage/mosd/usr/bin/mosd; \
    install -D -m 0644 /in/dist/mosd.service      /stage/mosd/usr/lib/systemd/system/mosd.service; \
    install -D -m 0644 /in/dist/com.mos.mosd.conf /stage/mosd/usr/share/dbus-1/system.d/com.mos.mosd.conf; \
    install -D -m 0644 /in/copyright              /stage/mosd/usr/share/doc/mosd/copyright; \
    install -d -m 0755 /stage/mosd/etc/systemd/system/multi-user.target.wants; \
    ln -s /usr/lib/systemd/system/mosd.service \
          /stage/mosd/etc/systemd/system/multi-user.target.wants/mosd.service; \
    install -D -m 0755 /in/bin/apid               /stage/apid/usr/bin/apid; \
    install -D -m 0644 /in/dist/apid.service      /stage/apid/usr/lib/systemd/system/apid.service; \
    install -D -m 0644 /in/copyright              /stage/apid/usr/share/doc/mos-apid/copyright; \
    install -d -m 0755 /stage/apid/etc/systemd/system/multi-user.target.wants; \
    ln -s /usr/lib/systemd/system/apid.service \
          /stage/apid/etc/systemd/system/multi-user.target.wants/apid.service; \
    pack.sh --root /stage/mosd --control /in/control/mosd.control \
            --version "${MOS_DEB_VERSION}" --arch "${MOS_DEB_ARCH}" --out /out; \
    pack.sh --root /stage/apid --control /in/control/mos-apid.control \
            --version "${MOS_DEB_VERSION}" --arch "${MOS_DEB_ARCH}" --out /out

# The export stage, the shape pkgs/mos-deploy/Dockerfile and
# pkgs/podman/Dockerfile end in: `-o type=local` writes this stage's
# filesystem into _out/debs/<arch>/pool/ and nothing here installs into a root.
FROM scratch AS out
COPY --from=pack /out/ /

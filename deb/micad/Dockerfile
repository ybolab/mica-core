# syntax=docker/dockerfile:1@sha256:ecfaec9ed6d810b56388c508f4121597bfbba70d41a6dfeee4d8cad5f295fc32
# check=skip=InvalidDefaultArgInFrom
# The micad producer: the `micad` and `mica-apid` packages -- one binary,
# /usr/bin/micad, and the /usr/bin/apid link to it.
#
#   make os-deb-micad   ->  build-env/deb/build.sh --producer micad --arch <amd64|arm64>
#
# This file only STAGES and PACKS. The binary was cross-compiled on an amd64
# Rust builder by this producer's PREPARE hook and arrives through the `bin`
# context; the shared copyright file arrives through `family`, because the build
# context is this producer's own directory. This
# stage runs at the TARGET architecture because dpkg-shlibdeps resolves an
# ELF's dependencies against the libraries of the container it runs in, and
# build-env/deb/pack.sh refuses the mismatch by name.
#
# No default for the base, for the reason build-env/deb/Dockerfile gives: a
# driver that failed to pass the argument would otherwise build from a floating
# tag.
ARG MICA_BUILD_DEB

FROM ${MICA_BUILD_DEB} AS pack

# pack.sh comes over as a named context rather than out of the image. It is the
# packaging CONTRACT, shared by every producer, and build-env/deb/README.md
# requires this shape: an edit to the packer then takes effect without
# rebuilding the whole builder family.
COPY --from=packer pack.sh /usr/local/bin/pack.sh

COPY --from=family copyright /in/copyright
COPY control/ /in/control/
COPY --from=dist micad.service apid.service com.mica.micad.conf /in/dist/
COPY --from=bin micad /in/bin/
# The OpenAPI document, staged beside the binaries by prepare.sh.
COPY --from=bin openapi.json /in/openapi.json

ARG MICA_DEB_VERSION
ARG MICA_DEB_ARCH
ARG SOURCE_DATE_EPOCH
# Provenance, written into the control file by pack.sh; see build-env/deb/README.md.
ARG MICA_DEB_SOURCE_REPO
ARG MICA_DEB_SOURCE_COMMIT

# Two staged roots and two pack.sh runs. The paths, the modes and the
# enablement links are the ones rootfs/scripts/micad-install.sh writes today,
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
    install -D -m 0755 /in/bin/micad               /stage/micad/usr/bin/micad; \
    install -D -m 0644 /in/dist/micad.service      /stage/micad/usr/lib/systemd/system/micad.service; \
    install -D -m 0644 /in/dist/com.mica.micad.conf /stage/micad/usr/share/dbus-1/system.d/com.mica.micad.conf; \
    install -D -m 0644 /in/copyright              /stage/micad/usr/share/doc/micad/copyright; \
    install -d -m 0755 /stage/micad/etc/systemd/system/multi-user.target.wants; \
    ln -s /usr/lib/systemd/system/micad.service \
          /stage/micad/etc/systemd/system/multi-user.target.wants/micad.service; \
    install -d -m 0755                            /stage/apid/usr/bin; \
    ln -s micad                                   /stage/apid/usr/bin/apid; \
    install -D -m 0644 /in/dist/apid.service      /stage/apid/usr/lib/systemd/system/apid.service; \
    install -D -m 0644 /in/copyright              /stage/apid/usr/share/doc/mica-apid/copyright; \
    install -D -m 0644 /in/openapi.json           /stage/apid/usr/share/mica-apid/openapi.json; \
    install -d -m 0755 /stage/apid/etc/systemd/system/multi-user.target.wants; \
    ln -s /usr/lib/systemd/system/apid.service \
          /stage/apid/etc/systemd/system/multi-user.target.wants/apid.service; \
    pack.sh --root /stage/micad --control /in/control/micad.control \
            --version "${MICA_DEB_VERSION}" --arch "${MICA_DEB_ARCH}" --out /out; \
    pack.sh --root /stage/apid --control /in/control/mica-apid.control \
            --version "${MICA_DEB_VERSION}" --arch "${MICA_DEB_ARCH}" --out /out

# The export stage, the shape pkgs/mica-deploy/Dockerfile and
# mica-podman:Dockerfile end in: `-o type=local` writes this stage's
# filesystem into _out/debs/<arch>/pool/ and nothing here installs into a root.
FROM scratch AS out
COPY --from=pack /out/ /

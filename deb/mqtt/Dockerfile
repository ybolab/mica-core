# syntax=docker/dockerfile:1@sha256:ecfaec9ed6d810b56388c508f4121597bfbba70d41a6dfeee4d8cad5f295fc32
# check=skip=InvalidDefaultArgInFrom
# The MQTT producer: the `mica-mqttd` and `mica-mqtt-broker` packages.
#
#   make os-deb-mqtt   ->  build-env/deb/build.sh --producer mqtt --arch <amd64|arm64>
#
# The shape is deb/micad/Dockerfile's: this file only STAGES and
# PACKS, the binaries were cross-compiled on an amd64 Rust builder by this
# producer's PREPARE hook and arrive through the `bin` context, and this stage runs at the TARGET
# architecture because dpkg-shlibdeps resolves an ELF's dependencies against the
# libraries of the container it runs in.
#
# TWO unit contexts rather than the micad producer's single `dist`. Each of these
# units lives beside its own crate -- mqttd/dist/ and
# broker/dist/ -- and the only directory containing both is the
# whole workspace, which is the cargo target tree buildx must not walk.
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
COPY maintainer/ /in/maintainer/
COPY --from=mqttd-dist mica-mqttd.service /in/dist/
COPY --from=broker-dist mica-mqtt-broker.service /in/dist/
COPY --from=bin mica-mqttd mica-mqtt-broker /in/bin/

ARG MOS_DEB_VERSION
ARG MOS_DEB_ARCH
ARG SOURCE_DATE_EPOCH
# Provenance, written into the control file by pack.sh; see build-env/deb/README.md.
ARG MOS_DEB_SOURCE_REPO
ARG MOS_DEB_SOURCE_COMMIT

# Two staged roots and two pack.sh runs. The paths and the modes are the ones
# rootfs/scripts/micad-install.sh writes today, so a root composed from these
# packages is the tree the stage chain installs.
#
# NEITHER PACKAGE SHIPS AN ENABLEMENT LINK, and that is the deliberate opposite
# of the micad producer beside it. micad owns the MQTT lifecycle: it renders
# /run/mica/mqtt-broker.toml and /run/mica/mqttd-device.env from the settings tree
# and then starts these units, and `mqtt.enabled` is the master switch. A
# multi-user.target.wants symlink in either payload would start a broker nobody
# asked for on every boot, from before micad has rendered anything for it to
# read -- which is the one thing that switch exists to prevent. Both units keep
# their [Install] section, so `systemctl enable` stays meaningful to anyone
# debugging on a writable root; nothing here creates the link, and no maintainer
# script calls `systemctl enable` either.
# rootfs/scripts/micad-install.sh encodes exactly this with `rm -f` followed
# by `test ! -e` for both units.
#
# The enrollment directory is mica-mqttd's and ships EMPTY. micad writes
# application enrollments into it at runtime, so the directory has to exist in
# the image; it belongs to the bridge that reads it and to no other package.
#
# The maintainer scripts create the two pinned service accounts, one per
# package, which is why each pack.sh run gets its own --maintainer-scripts
# directory. /usr/share/doc/<package>/copyright is per package as well, and no
# other path is staged into both roots: the two payloads are disjoint, and
# disjoint from micad's and mica-apid's.
RUN set -eu; \
    install -D -m 0755 /in/bin/mica-mqttd              /stage/mqttd/usr/bin/mica-mqttd; \
    install -D -m 0644 /in/dist/mica-mqttd.service     /stage/mqttd/usr/lib/systemd/system/mica-mqttd.service; \
    install -d -m 0755 /stage/mqttd/usr/lib/mica/mqtt-applications.d; \
    install -D -m 0644 /in/copyright                  /stage/mqttd/usr/share/doc/mica-mqttd/copyright; \
    install -D -m 0755 /in/bin/mica-mqtt-broker        /stage/broker/usr/bin/mica-mqtt-broker; \
    install -D -m 0644 /in/dist/mica-mqtt-broker.service /stage/broker/usr/lib/systemd/system/mica-mqtt-broker.service; \
    install -D -m 0644 /in/copyright                  /stage/broker/usr/share/doc/mica-mqtt-broker/copyright; \
    pack.sh --root /stage/mqttd --control /in/control/mica-mqttd.control \
            --maintainer-scripts /in/maintainer/mica-mqttd \
            --version "${MOS_DEB_VERSION}" --arch "${MOS_DEB_ARCH}" --out /out; \
    pack.sh --root /stage/broker --control /in/control/mica-mqtt-broker.control \
            --maintainer-scripts /in/maintainer/mica-mqtt-broker \
            --version "${MOS_DEB_VERSION}" --arch "${MOS_DEB_ARCH}" --out /out

# The export stage, the shape deb/micad/Dockerfile ends in:
# `-o type=local` writes this stage's filesystem into _out/debs/<arch>/pool/ and
# nothing here installs into a root.
FROM scratch AS out
COPY --from=pack /out/ /

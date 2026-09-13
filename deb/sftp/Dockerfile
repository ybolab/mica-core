# syntax=docker/dockerfile:1@sha256:ecfaec9ed6d810b56388c508f4121597bfbba70d41a6dfeee4d8cad5f295fc32
# check=skip=InvalidDefaultArgInFrom
# The sftp producer: the `mica-sftp-server` package -- one binary, installed as
# /usr/lib/sftp-server, the path dropbear-bin execs for the sftp subsystem.
#
#   make deb   ->  build-env/deb/build.sh --producer sftp --arch <amd64|arm64>
#
# The shape is deb/micad/Dockerfile's: this file only STAGES and PACKS, the
# binary was cross-compiled on an amd64 Rust builder by this producer's PREPARE
# hook and arrives through the `bin` context, and this stage runs at the TARGET
# architecture because dpkg-shlibdeps resolves an ELF's dependencies against
# the libraries of the container it runs in.
#
# No default for the base, for the reason build-env/deb/Dockerfile gives: a
# driver that failed to pass the argument would otherwise build from a floating
# tag.
ARG MICA_BUILD_DEB

FROM ${MICA_BUILD_DEB} AS pack

# pack.sh comes over as a named context rather than out of the image; see
# deb/micad/Dockerfile.
COPY --from=packer pack.sh /usr/local/bin/pack.sh

COPY --from=family copyright /in/copyright
COPY control/ /in/control/
COPY --from=bin mica-sftp-server /in/bin/

ARG MICA_DEB_VERSION
ARG MICA_DEB_ARCH
ARG SOURCE_DATE_EPOCH
# Provenance, written into the control file by pack.sh; see build-env/deb/README.md.
ARG MICA_DEB_SOURCE_REPO
ARG MICA_DEB_SOURCE_COMMIT

# /usr/lib/sftp-server and not /usr/lib/openssh/sftp-server: dropbear-bin is
# built with SFTPSERVER_PATH=/usr/lib/sftp-server. No unit, no enablement link
# and no maintainer script: the SSH server starts it per session.
RUN set -eu; \
    install -D -m 0755 /in/bin/mica-sftp-server /stage/sftp/usr/lib/sftp-server; \
    install -D -m 0644 /in/copyright            /stage/sftp/usr/share/doc/mica-sftp-server/copyright; \
    pack.sh --root /stage/sftp --control /in/control/mica-sftp-server.control \
            --version "${MICA_DEB_VERSION}" --arch "${MICA_DEB_ARCH}" --out /out

# The export stage: `-o type=local` writes this stage's filesystem into
# _out/debs/<arch>/pool/ and nothing here installs into a root.
FROM scratch AS out
COPY --from=pack /out/ /

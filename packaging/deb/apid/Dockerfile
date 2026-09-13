# syntax=docker/dockerfile:1@sha256:ecfaec9ed6d810b56388c508f4121597bfbba70d41a6dfeee4d8cad5f295fc32
# check=skip=InvalidDefaultArgInFrom
# The apid producer: the `mica-apid` package -- /usr/bin/mica-apid, its own
# executable, apid.service and the committed OpenAPI document.
#
#   scripts/deb/build.sh --producer apid --arch <amd64|arm64>
#
# The shape is packaging/deb/micad/Dockerfile's: this file only STAGES and
# PACKS; the binary was cross-compiled by this producer's PREPARE hook and
# arrives through the `bin` context, and this stage runs at the TARGET
# architecture because dpkg-shlibdeps resolves an ELF's dependencies against
# the libraries of the container it runs in.
ARG MICA_BUILD_DEB

FROM ${MICA_BUILD_DEB} AS pack

COPY --from=packer pack.sh /usr/local/bin/pack.sh

COPY --from=family copyright /in/copyright
COPY control/ /in/control/
COPY --from=apid-dist apid.service /in/dist/
COPY --from=bin mica-apid /in/bin/
# The OpenAPI document, staged beside the binary by prepare.sh.
COPY --from=bin openapi.json /in/openapi.json

ARG MICA_DEB_VERSION
ARG MICA_DEB_ARCH
ARG SOURCE_DATE_EPOCH
# Provenance, written into the control file by pack.sh; see scripts/deb/README.md.
ARG MICA_DEB_SOURCE_REPO
ARG MICA_DEB_SOURCE_COMMIT

# ENABLEMENT IS OWNED BY THE PACKAGE: the multi-user.target.wants symlink is a
# file in the payload, not something a maintainer script creates, and not a
# conffile (see packaging/deb/micad/Dockerfile).
RUN set -eu; \
    install -D -m 0755 /in/bin/mica-apid          /stage/apid/usr/bin/mica-apid; \
    install -D -m 0644 /in/dist/apid.service      /stage/apid/usr/lib/systemd/system/apid.service; \
    install -D -m 0644 /in/copyright              /stage/apid/usr/share/doc/mica-apid/copyright; \
    install -D -m 0644 /in/openapi.json           /stage/apid/usr/share/mica-apid/openapi.json; \
    install -d -m 0755 /stage/apid/etc/systemd/system/multi-user.target.wants; \
    ln -s /usr/lib/systemd/system/apid.service \
          /stage/apid/etc/systemd/system/multi-user.target.wants/apid.service; \
    pack.sh --root /stage/apid --control /in/control/mica-apid.control \
            --version "${MICA_DEB_VERSION}" --arch "${MICA_DEB_ARCH}" --out /out

FROM scratch AS out
COPY --from=pack /out/ /

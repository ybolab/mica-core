# syntax=docker/dockerfile:1@sha256:ecfaec9ed6d810b56388c508f4121597bfbba70d41a6dfeee4d8cad5f295fc32
# check=skip=InvalidDefaultArgInFrom
ARG MICA_BUILD_DEB
FROM ${MICA_BUILD_DEB} AS pack
COPY --from=packer pack.sh /usr/local/bin/pack.sh
COPY --from=family copyright /in/copyright
COPY control/ /in/control/
COPY --from=bin mica-deploy /in/mica-deploy
ARG MICA_DEB_VERSION
ARG MICA_DEB_ARCH
ARG SOURCE_DATE_EPOCH
# Provenance, written into the control file by pack.sh; see scripts/deb/README.md.
ARG MICA_DEB_SOURCE_REPO
ARG MICA_DEB_SOURCE_COMMIT
RUN set -eu; \
    install -D -m 0755 /in/mica-deploy /stage/usr/bin/mica-deploy; \
    install -D -m 0644 /in/copyright /stage/usr/share/doc/mica-deploy/copyright; \
    pack.sh --root /stage --control /in/control/mica-deploy.control \
        --version "${MICA_DEB_VERSION}" --arch "${MICA_DEB_ARCH}" --out /out
FROM scratch AS out
COPY --from=pack /out/ /

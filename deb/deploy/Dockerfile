# syntax=docker/dockerfile:1@sha256:ecfaec9ed6d810b56388c508f4121597bfbba70d41a6dfeee4d8cad5f295fc32
# check=skip=InvalidDefaultArgInFrom
ARG MOS_BUILD_DEB
FROM ${MOS_BUILD_DEB} AS pack
COPY --from=packer pack.sh /usr/local/bin/pack.sh
COPY --from=family copyright /in/copyright
COPY control/ /in/control/
COPY --from=bin mos-deploy /in/mos-deploy
ARG MOS_DEB_VERSION
ARG MOS_DEB_ARCH
ARG SOURCE_DATE_EPOCH
# Provenance, written into the control file by pack.sh; see build-env/deb/README.md.
ARG MOS_DEB_SOURCE_REPO
ARG MOS_DEB_SOURCE_COMMIT
RUN set -eu; \
    install -D -m 0755 /in/mos-deploy /stage/usr/bin/mos-deploy; \
    install -D -m 0644 /in/copyright /stage/usr/share/doc/mos-deploy/copyright; \
    pack.sh --root /stage --control /in/control/mos-deploy.control \
        --version "${MOS_DEB_VERSION}" --arch "${MOS_DEB_ARCH}" --out /out
FROM scratch AS out
COPY --from=pack /out/ /

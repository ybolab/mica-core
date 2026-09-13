# The lifecycle producer: mica-runkit, static, packed as mica-lifecycle. Same
# shape as pkgs/deploy/Dockerfile; the binary arrives through the `bin`
# context prepare.sh staged, already proven static and of
# this architecture by scripts/build/build-deb.sh.
ARG MICA_BUILD_DEB
FROM ${MICA_BUILD_DEB} AS pack
COPY --from=packer pack.sh /usr/local/bin/pack.sh
COPY --from=family copyright /in/copyright
COPY control/ /in/control/
COPY --from=bin mica-runkit /in/

ARG MICA_DEB_VERSION
ARG MICA_DEB_ARCH
ARG SOURCE_DATE_EPOCH
# Provenance, written into the control file by pack.sh; see scripts/deb/README.md.
ARG MICA_DEB_SOURCE_REPO
ARG MICA_DEB_SOURCE_COMMIT

RUN set -eu; \
    install -D -m 0755 /in/mica-runkit /stage/usr/lib/mica/lifecycle/mica-runkit; \
    install -D -m 0644 /in/copyright /stage/usr/share/doc/mica-lifecycle/copyright; \
    pack.sh --root /stage --control /in/control/mica-lifecycle.control \
        --version "${MICA_DEB_VERSION}" --arch "${MICA_DEB_ARCH}" --out /out

FROM scratch AS out
COPY --from=pack /out/ /

# The lifecycle producer: mica-init and mica-shutdown, static, packed as
# mica-lifecycle. Same shape as deb/deploy/Dockerfile; the binaries arrive
# through the `bin` context prepare.sh staged, already proven static and of
# this architecture by hack/build-deb.sh.
ARG MOS_BUILD_DEB
FROM ${MOS_BUILD_DEB} AS pack
COPY --from=packer pack.sh /usr/local/bin/pack.sh
COPY --from=family copyright /in/copyright
COPY control/ /in/control/
COPY --from=bin mica-init mica-shutdown /in/

ARG MOS_DEB_VERSION
ARG MOS_DEB_ARCH
ARG SOURCE_DATE_EPOCH
# Provenance, written into the control file by pack.sh; see build-env/deb/README.md.
ARG MOS_DEB_SOURCE_REPO
ARG MOS_DEB_SOURCE_COMMIT

RUN set -eu; \
    install -D -m 0755 /in/mica-init /stage/usr/lib/mica/lifecycle/mica-init; \
    install -D -m 0755 /in/mica-shutdown /stage/usr/lib/mica/lifecycle/mica-shutdown; \
    install -D -m 0644 /in/copyright /stage/usr/share/doc/mica-lifecycle/copyright; \
    pack.sh --root /stage --control /in/control/mica-lifecycle.control \
        --version "${MOS_DEB_VERSION}" --arch "${MOS_DEB_ARCH}" --out /out

FROM scratch AS out
COPY --from=pack /out/ /

# syntax=docker/dockerfile:1@sha256:ecfaec9ed6d810b56388c508f4121597bfbba70d41a6dfeee4d8cad5f295fc32
# check=skip=InvalidDefaultArgInFrom
ARG MOS_BUILD_DEB
FROM ${MOS_BUILD_DEB} AS pack
COPY --from=packer pack.sh /usr/local/bin/pack.sh
COPY --from=family copyright /in/copyright
COPY --from=reference . /stage/
COPY --from=bin mos-mqtt-reference /in/bin/mos-mqtt-reference
COPY control/ /in/control/
ARG MOS_DEB_VERSION
ARG MOS_DEB_ARCH
ARG SOURCE_DATE_EPOCH
RUN set -eu; \
    install -D -m 0755 /in/bin/mos-mqtt-reference /stage/usr/bin/mos-mqtt-reference; \
    rm /stage/usr/lib/systemd/system/multi-user.target.d/mos-mqtt-reference.conf; \
    rmdir /stage/usr/lib/systemd/system/multi-user.target.d; \
    install -d /stage/etc/systemd/system/multi-user.target.wants; \
    ln -s /usr/lib/systemd/system/mos-mqtt-reference.service /stage/etc/systemd/system/multi-user.target.wants/; \
    install -D -m 0644 /in/copyright /stage/usr/share/doc/mos-mqtt-reference/copyright; \
    pack.sh --root /stage --control /in/control/mos-mqtt-reference.control \
        --version "$MOS_DEB_VERSION" --arch "$MOS_DEB_ARCH" --out /out
FROM scratch AS out
COPY --from=pack /out/ /

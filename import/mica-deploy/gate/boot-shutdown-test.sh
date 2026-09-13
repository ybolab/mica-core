#!/usr/bin/env bash
# Deterministic lifecycle/descriptor and compile-only ABI fixtures; no devices or guest.
set -euo pipefail
for tool in docker timeout bash dirname mkdir uname; do command -v "$tool" >/dev/null; done
ARM_ABI=0
case "${1:-}" in
    "") ;;
    --arm-abi) ARM_ABI=1 ;;
    *) echo 'Usage: boot-shutdown-test.sh [--arm-abi]' >&2; exit 2 ;;
esac
REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
case "$(uname -m)" in x86_64) ;; *) echo 'This fixture runner requires the pinned amd64 build container' >&2; exit 1;; esac
IMAGE=$(bash "$REPO/build-env/from.sh" --arch=amd64 --ref LOCAL_MICA_BUILD_RUST_CHECK)
HOST_REPO=$REPO
case "$REPO" in /root/*) HOST_REPO="/srv/station/root/${REPO#/root/}";; /work/*) HOST_REPO="/srv/station/work/${REPO#/work/}";; esac
docker image inspect --format '{{.Id}}' "$IMAGE"
# The target directory is this suite's own; the crate cache is the one every
# other cargo run in this repository fills (gate/rust-gate.sh, hack/build-deb.sh),
# because the build below is --offline and a private empty registry would
# resolve nothing on a fresh clone.
CACHE="$REPO/_out/b3-rust"
mkdir -p "$CACHE/target" "$REPO/_out/cargo/registry" "$REPO/_out/cargo/git"
# /srv paths map identically; /root and /work are translated above for siblings.
# mica-build-side: container-block -- pinned native and UAPI fixture toolchain.
timeout 110 docker run --rm --label ai-agent=true --network traefik \
    --name "ai-agent-mos-boot-shutdown-$$" \
    -v "$HOST_REPO:/src:ro" -v "$HOST_REPO/_out/b3-rust/target:/target" \
    -v "$HOST_REPO/_out/cargo/registry:/usr/local/cargo/registry" \
    -v "$HOST_REPO/_out/cargo/git:/usr/local/cargo/git" \
    -e "ARM_ABI=$ARM_ABI" -e CARGO_TARGET_DIR=/target -w /src --entrypoint /bin/bash "$IMAGE" -c '
set -euo pipefail
for tool in cargo gcc; do command -v "$tool" >/dev/null; done
cargo test --locked --offline --lib boot::shutdown::tests
cargo test --locked --offline --test shutdown --test exitrd --test startup --test boot
cargo test --locked --offline -p lifecycle-sys
compilers=(gcc)
if [ "$ARM_ABI" = 1 ]; then
    command -v aarch64-linux-gnu-gcc >/dev/null
    compilers+=(aarch64-linux-gnu-gcc)
fi
for compiler in "${compilers[@]}"; do
    "$compiler" -std=c11 -Wall -Werror -c /src/gate/boot-shutdown/uapi.c -o "/target/b3-uapi-$compiler.o"
    "$compiler" -dumpfullversion
done
dpkg-query -W "linux-libc-dev*"
sha256sum /usr/include/linux/dm-ioctl.h /usr/include/linux/loop.h /usr/include/linux/watchdog.h /usr/include/asm-generic/ioctl.h
if [ "$ARM_ABI" = 1 ]; then
    sha256sum /usr/aarch64-linux-gnu/include/linux/dm-ioctl.h /usr/aarch64-linux-gnu/include/linux/loop.h \
        /usr/aarch64-linux-gnu/include/linux/watchdog.h /usr/aarch64-linux-gnu/include/asm-generic/ioctl.h
else
    printf "%s\n" "ARM_ABI_DEFERRED: explicit --arm-abi required after the user-approved main merge"
fi
printf "%s\n" BOOT_SHUTDOWN_FIXTURES_PASS
'
# mica-build-side: host
